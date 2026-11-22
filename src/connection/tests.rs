use std::{
    os::unix::net::{UnixListener, UnixStream},
    sync::atomic::{AtomicUsize, Ordering},
};

use async_io::Async;
use futures_lite::StreamExt;

use super::*;
use crate::wire::Frame;

const SERVICE: &str = "org.ls2.test.peer";
const METHOD: &str = "luna://org.ls2.test.peer/test/echo";

static TEST_ID: AtomicUsize = AtomicUsize::new(0);

struct TestHub {
    listener: Async<UnixListener>,
    path: PathBuf,
}

impl TestHub {
    fn new() -> Self {
        let id = TEST_ID.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("ls2-{}-{id}.sock", std::process::id()));
        let listener = Async::<UnixListener>::bind(&path).expect("bind test hub");
        Self { listener, path }
    }

    fn client(&self) -> ConnectionBuilder {
        Connection::builder().socket_path(&self.path)
    }

    async fn accept_client(&self) -> Socket {
        let (stream, _) = self.listener.accept().await.expect("accept Luna client");
        let socket = Socket::from_fd(
            stream.into_inner().expect("extract stream").into(),
            crate::ProtocolProfile::DEFAULT,
        )
        .expect("adopt test socket");
        let request = read(&socket).await;
        assert_eq!(request.kind, Kind::RequestName);
        let mut body = Vec::new();
        for value in [0, 0, 0] {
            argument_int(&mut body, value);
        }
        argument_string(&mut body, Some("test-client"));
        argument_string(&mut body, Some("[]"));
        argument_int(&mut body, 0);
        argument_string(&mut body, Some("[]"));
        argument_string(&mut body, Some("public"));
        body.extend_from_slice(&[0; 8]);
        write(&socket, &Frame::new(Kind::RequestNameReply, 1, false, body)).await;
        write(
            &socket,
            &Frame::new(Kind::MonitorNotConnected, 2, false, vec![0; 20]),
        )
        .await;
        socket
    }
}

impl Drop for TestHub {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

async fn read(socket: &Socket) -> Frame {
    socket.recv().await.expect("read test frame")
}

async fn write(socket: &Socket, frame: &Frame) {
    socket.send(frame).await.expect("write test frame");
}

async fn offer_peer(hub: &Socket) -> Socket {
    let frame = read(hub).await;
    assert_eq!(frame.kind, Kind::QueryName);
    let (remote, client) = UnixStream::pair().expect("create test peer sockets");
    let mut body = Vec::new();
    argument_int(&mut body, 0);
    argument_string(&mut body, Some(SERVICE));
    argument_string(&mut body, Some("test-peer"));
    argument_int(&mut body, 0);
    argument_string(&mut body, None);
    argument_string(&mut body, Some("[]"));
    argument_int(&mut body, 2);
    argument_string(&mut body, Some("public"));
    argument_string(&mut body, Some("public"));
    argument_string(&mut body, None);
    body.extend_from_slice(&[0; 8]);
    let mut offer = Frame::new(Kind::QueryNameReply, 3, false, body);
    offer.fd = Some(client.into());
    write(hub, &offer).await;
    let peer = Socket::from_fd(remote.into(), crate::ProtocolProfile::DEFAULT)
        .expect("adopt test peer socket");
    assert_eq!(read(hub).await.kind, Kind::ClientConnected);
    assert_eq!(read(&peer).await.kind, Kind::ClientConnected);
    peer
}

fn argument_int(body: &mut Vec<u8>, value: i32) {
    body.extend_from_slice(&2_u32.to_le_bytes());
    body.extend_from_slice(&4_u32.to_le_bytes());
    body.extend_from_slice(&value.to_le_bytes());
}

fn argument_string(body: &mut Vec<u8>, value: Option<&str>) {
    let size = value.map_or(0, |value| value.len() + 1);
    let padded = size.next_multiple_of(4);
    body.extend_from_slice(&1_u32.to_le_bytes());
    body.extend_from_slice(&u32::try_from(padded).expect("padded size").to_le_bytes());
    body.extend_from_slice(&u32::try_from(size).expect("string size").to_le_bytes());
    if let Some(value) = value {
        body.extend_from_slice(value.as_bytes());
        body.resize(body.len() + padded - value.len(), 0);
    }
}

async fn send_reply(peer: &Socket, serial: u32, call: u32, payload: &str) {
    write(
        peer,
        &wire::reply(
            crate::ProtocolProfile::DEFAULT,
            Kind::Reply,
            serial,
            call,
            payload,
        )
        .expect("encode test reply"),
    )
    .await;
}

async fn joined(client: impl Future<Output = ()>, server: impl Future<Output = ()>) {
    future::or(
        async {
            future::zip(client, server).await;
        },
        async {
            Timer::after(Duration::from_secs(4)).await;
            panic!("test hub and client did not finish");
        },
    )
    .await;
}

#[tokio::test]
async fn concurrent_calls_route_replies_by_token_and_cancel_one_shots() {
    let hub = TestHub::new();
    let (verified, done) = async_channel::bounded(1);
    joined(
        async {
            let bus = hub.client().connect().await.expect("connect Tokio client");
            let (first, second) = future::zip(
                bus.call_raw(METHOD, "  first\n"),
                bus.call_raw(METHOD, "second"),
            )
            .await;
            assert_eq!(first.expect("first call"), "  first\n");
            assert_eq!(second.expect("second call"), "second");
            done.recv().await.expect("peer observed cancellation");
            bus.close().await;
        },
        async {
            let socket = hub.accept_client().await;
            let peer = offer_peer(&socket).await;
            let first = read(&peer).await;
            let second = read(&peer).await;
            for call in [&second, &first] {
                let payload = wire::parse_method_call(crate::ProtocolProfile::DEFAULT, call)
                    .expect("decode test call")
                    .payload;
                send_reply(&peer, 30, call.token, payload).await;
            }
            let mut cancelled = Vec::new();
            for _ in 0..2 {
                let frame = read(&peer).await;
                assert_eq!(frame.kind, Kind::CancelMethodCall);
                cancelled.push(wire::parse_cancel(&frame).expect("decode cancellation"));
            }
            assert!(cancelled.contains(&first.token));
            assert!(cancelled.contains(&second.token));
            verified.send(()).await.expect("confirm cancellation");
        },
    )
    .await;
}

#[test]
fn subscription_drop_cancels_and_disconnect_terminates_pending_work() {
    smol::block_on(async {
        let hub = TestHub::new();
        joined(
            async {
                let bus = hub.client().connect().await.expect("connect smol client");
                let mut stream = bus
                    .subscribe_raw(METHOD, "{}")
                    .await
                    .expect("start subscription");
                let first = stream
                    .next()
                    .await
                    .expect("initial item")
                    .expect("initial result");
                assert_eq!(first, "first");
                drop(stream);
                let error = bus
                    .call_raw(METHOD, "pending")
                    .await
                    .expect_err("peer disconnected");
                assert_eq!(error.kind(), ErrorKind::Disconnected);
            },
            async {
                let socket = hub.accept_client().await;
                let peer = offer_peer(&socket).await;
                let start = read(&peer).await;
                send_reply(&peer, 30, start.token, "first").await;
                send_reply(&peer, 31, start.token, "extra").await;
                let mut saw_cancel = false;
                for _ in 0..2 {
                    let frame = read(&peer).await;
                    if frame.kind == Kind::CancelMethodCall {
                        assert_eq!(
                            wire::parse_cancel(&frame).expect("decode cancel"),
                            start.token
                        );
                        saw_cancel = true;
                    } else {
                        assert_eq!(frame.kind, Kind::MethodCall);
                    }
                }
                assert!(
                    saw_cancel,
                    "dropping the subscription must cancel the peer call"
                );
            },
        )
        .await;
    });
}

#[test]
fn signal_listeners_share_one_hub_registration() {
    smol::block_on(async {
        let hub = TestHub::new();
        let (listening, ready) = async_channel::bounded(1);
        joined(
            async {
                let bus = hub.client().connect().await.expect("connect signal client");
                let filter = SignalFilter::exact("/test", "event").expect("valid filter");
                let (first, second) =
                    future::zip(bus.watch_signal(filter.clone()), bus.watch_signal(filter)).await;
                let mut first = first.expect("first listener");
                let mut second = second.expect("second listener");
                listening.send(()).await.expect("confirm both listeners");
                for listener in [&mut first, &mut second] {
                    let signal = listener
                        .next()
                        .await
                        .expect("signal item")
                        .expect("valid signal");
                    assert_eq!(
                        (
                            signal.category(),
                            signal.method(),
                            signal.payload().as_str()
                        ),
                        ("/test", "event", " broadcast ")
                    );
                }
                first.cancel().await.expect("cancel first listener");
                second.cancel().await.expect("unregister final listener");
            },
            async {
                let socket = hub.accept_client().await;
                let registration = read(&socket).await;
                assert_eq!(registration.kind, Kind::SignalRegister);
                send_reply(&socket, 10, registration.token, r#"{"returnValue":true}"#).await;
                ready.recv().await.expect("wait for both listeners");
                let signal =
                    wire::signal(11, "/test", "event", " broadcast ").expect("encode signal");
                write(&socket, &signal).await;
                assert_eq!(read(&socket).await.kind, Kind::SignalUnregister);
            },
        )
        .await;
    });
}

#[cfg(feature = "serde")]
#[tokio::test]
async fn serde_round_trip_and_decode_errors() {
    let hub = TestHub::new();
    joined(
        async {
            let bus = hub.client().connect().await.expect("connect typed client");
            let result: Vec<u32> = bus.call_json(METHOD, &[1, 2]).await.expect("typed call");
            assert_eq!(result, [1, 2]);
            let error = bus
                .call_json::<String>(METHOD, &())
                .await
                .expect_err("malformed JSON");
            assert_eq!(error.kind(), ErrorKind::Json);
        },
        async {
            let socket = hub.accept_client().await;
            let peer = offer_peer(&socket).await;
            let mut calls = 0;
            while calls < 2 {
                let frame = read(&peer).await;
                if frame.kind == Kind::MethodCall {
                    calls += 1;
                    let method = wire::parse_method_call(crate::ProtocolProfile::DEFAULT, &frame)
                        .expect("decode typed request");
                    let result = if calls == 1 {
                        method.payload
                    } else {
                        "malformed"
                    };
                    send_reply(&peer, 25, frame.token, result).await;
                }
            }
            let _ = peer.recv().await;
        },
    )
    .await;
}
