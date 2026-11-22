use std::io::{self, IoSlice, IoSliceMut, Read, Write};
use std::mem::MaybeUninit;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::Path;

use async_io::Async;
use rustix::net::{
    self, AddressFamily, RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, ReturnFlags,
    SendAncillaryBuffer, SendAncillaryMessage, SendFlags, SocketType, sockopt,
};

use super::{Frame, HEADER_LEN, Kind, MAX_BODY_LEN};

/// A full-duplex LS2 byte stream. Use one receive task and one send task per socket.
#[derive(Debug)]
pub(crate) struct Socket {
    stream: Async<UnixStream>,
}

impl Socket {
    pub(crate) async fn connect(path: &Path) -> io::Result<Self> {
        let stream = Async::<UnixStream>::connect(path).await?;
        #[cfg(target_vendor = "apple")]
        sockopt::set_socket_nosigpipe(stream.get_ref(), true)?;
        Ok(Self { stream })
    }

    pub(crate) fn from_fd(fd: OwnedFd) -> io::Result<Self> {
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
        })
    }

    pub(crate) async fn recv(&self) -> io::Result<Frame> {
        let mut header = [0; HEADER_LEN];
        self.read_exact(&mut header).await?;
        let (len, token, kind, public) = decode_header(header)?;
        let mut body = vec![0; len];
        self.read_exact(&mut body).await?;

        // The FD travels on a separate byte after the frame, even on a failed lookup.
        let fd = if kind.has_fd_slot() {
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
        let header = encode_header(frame)?;
        if frame.fd.is_some() && !frame.kind.has_fd_slot() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "this LS2 message kind cannot carry a descriptor",
            ));
        }

        self.write_all(&header).await?;
        self.write_all(&frame.body).await?;
        if frame.kind.has_fd_slot() {
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

fn encode_header(frame: &Frame) -> io::Result<[u8; HEADER_LEN]> {
    let mut header = [0; HEADER_LEN];
    let len = u32::try_from(frame.body.len())
        .ok()
        .filter(|len| (*len as usize) <= MAX_BODY_LEN)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "LS2 frame is too large"))?;
    header[0..4].copy_from_slice(&len.to_le_bytes());
    header[4..8].copy_from_slice(&frame.token.to_le_bytes());
    header[8..12].copy_from_slice(&frame.kind.to_wire().to_le_bytes());
    header[12] = u8::from(frame.public);
    Ok(header)
}

fn decode_header(header: [u8; HEADER_LEN]) -> io::Result<(usize, u32, Kind, bool)> {
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
    Ok((len, word(4), Kind::from_wire(word(8)), public))
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
            Socket::from_fd(first.into()).unwrap(),
            Socket::from_fd(second.into()).unwrap(),
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
                .send(&Frame::new(Kind::Unknown(35), 3, false, Vec::new()))
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
            assert_eq!(after.kind, Kind::Unknown(35));
            assert_eq!(after.token, 3);
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
