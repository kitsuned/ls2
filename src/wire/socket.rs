use std::io::{self, IoSlice, IoSliceMut, Read, Write};
use std::mem::MaybeUninit;
use std::os::fd::{AsFd, OwnedFd};
#[cfg(target_os = "linux")]
use std::os::unix::net::UnixListener;
use std::os::unix::net::UnixStream;
use std::path::Path;

use async_io::Async;
use rustix::net::{
    self, AddressFamily, RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, ReturnFlags,
    SendAncillaryBuffer, SendAncillaryMessage, SendFlags, SocketType, sockopt,
};

use super::{Frame, Kind, MAX_BODY_LEN};
use crate::ProtocolProfile;

const HEADER_LEN: usize = 16;
const SHORT_HEADER_LEN: usize = 12;

/// A full-duplex LS2 byte stream. Use one receive task and one send task per socket.
#[derive(Debug)]
pub(crate) struct Socket {
    stream: Async<UnixStream>,
    profile: ProtocolProfile,
}

impl Socket {
    pub(crate) async fn connect(path: &Path, profile: ProtocolProfile) -> io::Result<Self> {
        let stream = Async::<UnixStream>::connect(path).await?;
        #[cfg(target_vendor = "apple")]
        sockopt::set_socket_nosigpipe(stream.get_ref(), true)?;
        Ok(Self { stream, profile })
    }

    pub(crate) fn from_fd(fd: OwnedFd, profile: ProtocolProfile) -> io::Result<Self> {
        if sockopt::socket_type(&fd)? != SocketType::STREAM
            || net::getsockname(&fd)?.address_family() != AddressFamily::UNIX
        {
            return Err(invalid_data("LS2 peer descriptor is not a Unix stream"));
        }
        net::getpeername(&fd)?;
        #[cfg(target_vendor = "apple")]
        sockopt::set_socket_nosigpipe(&fd, true)?;
        Ok(Self {
            stream: Async::new(UnixStream::from(fd))?,
            profile,
        })
    }

    pub(crate) fn profile(&self) -> ProtocolProfile {
        self.profile
    }

    fn header_len(&self) -> usize {
        if self.profile.has_bus_flag() {
            HEADER_LEN
        } else {
            SHORT_HEADER_LEN
        }
    }

    pub(crate) async fn recv(&self) -> io::Result<Frame> {
        let mut header = [0; HEADER_LEN];
        self.read_exact(&mut header[..self.header_len()]).await?;
        let (len, token, kind, public) = decode_header(self.profile, header)?;
        let mut body = vec![0; len];
        self.read_exact(&mut body).await?;

        // The FD travels on a separate byte after the frame, even on a failed lookup.
        let fd = if self.profile.has_descriptor(kind) {
            self.stream
                .read_with(|stream| retry_interrupted(|| recv_descriptor(stream)))
                .await?
        } else {
            None
        };

        Ok(Frame {
            kind,
            token,
            public,
            body,
            fd,
        })
    }

    pub(crate) async fn send(&self, frame: &Frame) -> io::Result<()> {
        let header = encode_header(self.profile, frame)?;
        if frame.fd.is_some() && !self.profile.has_descriptor(frame.kind) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "this LS2 message kind cannot carry a descriptor",
            ));
        }

        self.write_all(&header[..self.header_len()]).await?;
        self.write_all(&frame.body).await?;
        if self.profile.has_descriptor(frame.kind) {
            self.stream
                .write_with(|stream| {
                    retry_interrupted(|| send_descriptor(stream, frame.fd.as_ref()))
                })
                .await?;
        }
        Ok(())
    }

    async fn read_exact(&self, mut remaining: &mut [u8]) -> io::Result<()> {
        while !remaining.is_empty() {
            let bytes = self
                .stream
                .read_with(|mut stream| retry_interrupted(|| stream.read(remaining)))
                .await?;
            if bytes == 0 {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            remaining = &mut remaining[bytes..];
        }
        Ok(())
    }

    async fn write_all(&self, mut remaining: &[u8]) -> io::Result<()> {
        while !remaining.is_empty() {
            let bytes = self
                .stream
                .write_with(|mut stream| retry_interrupted(|| stream.write(remaining)))
                .await?;
            if bytes == 0 {
                return Err(io::ErrorKind::WriteZero.into());
            }
            remaining = &remaining[bytes..];
        }
        Ok(())
    }
}

pub(crate) struct Listener {
    #[cfg(target_os = "linux")]
    listener: Async<UnixListener>,
    #[cfg(target_os = "linux")]
    profile: ProtocolProfile,
    #[cfg(target_os = "linux")]
    hub: net::UCred,
}

#[cfg(target_os = "linux")]
impl Listener {
    pub(crate) fn from_hub(fd: OwnedFd, hub: &Socket) -> io::Result<Self> {
        if sockopt::socket_type(&fd)? != SocketType::STREAM
            || net::getsockname(&fd)?.address_family() != AddressFamily::UNIX
            || !sockopt::socket_acceptconn(&fd)?
        {
            return Err(invalid_data(
                "LS2 registration descriptor is not a Unix listener",
            ));
        }
        Ok(Self {
            listener: Async::new(UnixListener::from(fd))?,
            profile: hub.profile,
            hub: sockopt::socket_peercred(hub.stream.get_ref())?,
        })
    }

    pub(crate) async fn accept(&self) -> io::Result<Socket> {
        loop {
            let (stream, _) = self.listener.accept().await?;
            // The old hub connects on behalf of an authorized caller, then transfers
            // its end. The kernel credentials on our accepted end remain the hub's.
            if !sockopt::socket_peercred(stream.get_ref()).is_ok_and(|peer| {
                peer.pid == self.hub.pid && peer.uid == self.hub.uid && peer.gid == self.hub.gid
            }) {
                continue;
            }
            return Ok(Socket {
                stream,
                profile: self.profile,
            });
        }
    }
}

#[cfg(not(target_os = "linux"))]
impl Listener {
    pub(crate) fn from_hub(_fd: OwnedFd, _hub: &Socket) -> io::Result<Self> {
        Err(Self::unsupported())
    }

    pub(crate) async fn accept(&self) -> io::Result<Socket> {
        Err(Self::unsupported())
    }

    fn unsupported() -> io::Error {
        io::Error::new(
            io::ErrorKind::Unsupported,
            "legacy Luna listener registration requires Linux peer credentials",
        )
    }
}

fn encode_header(profile: ProtocolProfile, frame: &Frame) -> io::Result<[u8; HEADER_LEN]> {
    let mut header = [0; HEADER_LEN];
    let len = u32::try_from(frame.body.len())
        .ok()
        .filter(|len| (*len as usize) <= MAX_BODY_LEN)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "LS2 frame is too large"))?;
    header[0..4].copy_from_slice(&len.to_le_bytes());
    header[4..8].copy_from_slice(&frame.token.to_le_bytes());
    header[8..12].copy_from_slice(&profile.encode_kind(frame.kind)?.to_le_bytes());
    if profile.has_bus_flag() {
        header[12] = u8::from(frame.public);
    }
    Ok(header)
}

fn decode_header(
    profile: ProtocolProfile,
    header: [u8; HEADER_LEN],
) -> io::Result<(usize, u32, Kind, bool)> {
    let word = |offset| {
        let mut bytes = [0; 4];
        bytes.copy_from_slice(&header[offset..offset + 4]);
        u32::from_le_bytes(bytes)
    };
    let len = word(0) as usize;
    if len > MAX_BODY_LEN {
        return Err(invalid_data("LS2 frame is too large"));
    }
    let public = match header[12] {
        0 => false,
        1 => true,
        _ => return Err(invalid_data("invalid LS2 header bus flag")),
    };
    Ok((len, word(4), profile.decode_kind(word(8))?, public))
}

fn recv_descriptor(stream: &UnixStream) -> io::Result<Option<OwnedFd>> {
    #[cfg(target_os = "linux")]
    const FLAGS: RecvFlags = RecvFlags::CMSG_CLOEXEC;
    #[cfg(not(target_os = "linux"))]
    const FLAGS: RecvFlags = RecvFlags::empty();

    let mut marker = [0];
    let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(2))];
    let mut control = RecvAncillaryBuffer::new(&mut space);
    let response = net::recvmsg(
        stream,
        &mut [IoSliceMut::new(&mut marker)],
        &mut control,
        FLAGS,
    )?;
    if response.bytes == 0 {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }
    if response.flags.contains(ReturnFlags::CTRUNC) {
        return Err(invalid_data("truncated LS2 descriptor data"));
    }

    let mut fd = None;
    for message in control.drain() {
        if let RecvAncillaryMessage::ScmRights(rights) = message {
            for received in rights {
                if fd.replace(received).is_some() {
                    return Err(invalid_data("LS2 frame carried more than one descriptor"));
                }
            }
        }
    }
    match (marker[0], fd) {
        (0, Some(fd)) => {
            #[cfg(not(target_os = "linux"))]
            rustix::io::fcntl_setfd(&fd, rustix::io::FdFlags::CLOEXEC)?;
            Ok(Some(fd))
        }
        (0, None) => Err(invalid_data("LS2 frame is missing a descriptor")),
        (_, Some(_)) => Err(invalid_data("unexpected descriptor in LS2 frame")),
        (_, None) => Ok(None),
    }
}

fn send_descriptor(stream: &UnixStream, fd: Option<&OwnedFd>) -> io::Result<()> {
    #[cfg(not(target_vendor = "apple"))]
    const FLAGS: SendFlags = SendFlags::NOSIGNAL;
    #[cfg(target_vendor = "apple")]
    const FLAGS: SendFlags = SendFlags::empty();

    let marker = [u8::from(fd.is_none())];
    let fds = fd.map(|fd| [fd.as_fd()]);
    let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(1))];
    let mut control = SendAncillaryBuffer::new(&mut space);
    if let Some(fds) = &fds {
        if !control.push(SendAncillaryMessage::ScmRights(fds)) {
            return Err(io::Error::other(
                "could not construct descriptor control message",
            ));
        }
    }
    if net::sendmsg(stream, &[IoSlice::new(&marker)], &mut control, FLAGS)? != 1 {
        return Err(io::ErrorKind::WriteZero.into());
    }
    Ok(())
}

fn retry_interrupted<T>(mut operation: impl FnMut() -> io::Result<T>) -> io::Result<T> {
    loop {
        match operation() {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            result => return result,
        }
    }
}

fn invalid_data(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
    use futures_lite::future;

    use super::*;

    fn pair() -> (Socket, Socket) {
        let (first, second) = UnixStream::pair().unwrap();
        (
            Socket::from_fd(first.into(), ProtocolProfile::DEFAULT).unwrap(),
            Socket::from_fd(second.into(), ProtocolProfile::DEFAULT).unwrap(),
        )
    }

    #[test]
    fn fragmented_frame_has_the_arm32_layout() {
        async_io::block_on(async {
            let (reader, writer) = pair();
            let split_write = async {
                // Native frames may carry arbitrary alignment padding after the bus byte.
                writer.write_all(&[3, 0, 0, 0, 42]).await.unwrap();
                future::yield_now().await;
                writer
                    .write_all(&[0, 0, 0, 4, 0, 0, 0, 1, 11, 22, 33, 10])
                    .await
                    .unwrap();
                future::yield_now().await;
                writer.write_all(&[20, 30]).await.unwrap();
            };

            let (frame, ()) = future::zip(reader.recv(), split_write).await;
            let frame = frame.unwrap();
            assert_eq!(frame.kind, Kind::Signal);
            assert_eq!(frame.token, 42);
            assert!(frame.public);
            assert_eq!(frame.body, [10, 20, 30]);
        });
    }

    #[test]
    fn descriptor_trailer_preserves_the_next_frame_boundary() {
        async_io::block_on(async {
            let (reader, writer) = pair();
            let (offered, mut counterpart) = UnixStream::pair().unwrap();
            let mut success = Frame::new(Kind::QueryNameReply, 1, false, vec![0]);
            success.fd = Some(offered.into());
            writer.send(&success).await.unwrap();
            writer
                .send(&Frame::new(Kind::QueryNameReply, 2, false, vec![1]))
                .await
                .unwrap();
            writer
                .send(&Frame::new(Kind::ClientConnected, 3, false, Vec::new()))
                .await
                .unwrap();

            let fd = reader.recv().await.unwrap().fd.unwrap();
            assert!(
                rustix::io::fcntl_getfd(&fd)
                    .unwrap()
                    .contains(rustix::io::FdFlags::CLOEXEC)
            );
            let mut offered = UnixStream::from(fd);
            counterpart.write_all(b"ok").unwrap();
            let mut bytes = [0; 2];
            offered.read_exact(&mut bytes).unwrap();
            assert_eq!(&bytes, b"ok");

            let failure = reader.recv().await.unwrap();
            assert_eq!(failure.token, 2);
            assert!(failure.fd.is_none());
            let after = reader.recv().await.unwrap();
            assert_eq!(after.kind, Kind::ClientConnected);
            assert_eq!(after.token, 3);
        });
    }

    #[test]
    fn legacy_node_up_does_not_consume_the_following_descriptor_frame() {
        async_io::block_on(async {
            for profile in [
                ProtocolProfile::LocalListener,
                ProtocolProfile::InitiatorInfo,
            ] {
                let (first, second) = UnixStream::pair().unwrap();
                let reader = Socket::from_fd(first.into(), profile).unwrap();
                let writer = Socket::from_fd(second.into(), profile).unwrap();
                let (offered, _peer) = UnixStream::pair().unwrap();
                let mut offer = Frame::new(Kind::QueryNameReply, 2, false, vec![1, 2, 3]);
                offer.fd = Some(offered.into());

                // Type 3 means NodeUp in both legacy numberings. Later TVs use
                // that number for an FD-bearing reply.
                writer.write_all(&[0, 0, 0, 0, 1]).await.unwrap();
                let mut rest = vec![0, 0, 0, 3, 0, 0, 0];
                if profile.has_bus_flag() {
                    rest.extend_from_slice(&[0; 4]);
                }
                writer.write_all(&rest).await.unwrap();
                writer.send(&offer).await.unwrap();
                writer
                    .send(&Frame::new(Kind::Signal, 3, false, vec![4]))
                    .await
                    .unwrap();

                let next_frame = || {
                    future::race(reader.recv(), async {
                        async_io::Timer::after(std::time::Duration::from_secs(1)).await;
                        Err(io::ErrorKind::TimedOut.into())
                    })
                };
                let ready = next_frame().await.unwrap();
                assert_eq!(ready.kind, Kind::NodeUp);
                assert!(ready.fd.is_none());
                let offered = next_frame().await.unwrap();
                assert_eq!(offered.kind, Kind::QueryNameReply);
                assert!(offered.fd.is_some());
                assert_eq!(next_frame().await.unwrap().body, [4]);
            }
        });
    }

    #[test]
    fn oversize_frame_is_rejected_before_reading_its_body() {
        async_io::block_on(async {
            let (reader, writer) = pair();
            let mut header = [0; HEADER_LEN];
            header[..4].copy_from_slice(&u32::MAX.to_le_bytes());
            writer.write_all(&header).await.unwrap();
            assert_eq!(
                reader.recv().await.unwrap_err().kind(),
                io::ErrorKind::InvalidData,
            );
        });
    }
}
