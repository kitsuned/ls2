//! Exercises the Rust end of the on-device LS2 interoperability fixtures.

use std::error::Error;
use std::future;
use std::io;
use std::time::Duration;

use futures_lite::{StreamExt, stream};
use ls2::{Connection, ConnectionBuilder, ErrorKind, ProtocolProfile, Reply, Router, SignalFilter};

const RUST_NAME: &str = "com.webos.ls2.test.rust";
const NATIVE_ECHO: &str = "luna://com.webos.ls2.test.native/test/echo";
const NATIVE_UPDATES: &str = "luna://com.webos.ls2.test.native/test/updates";
const NATIVE_SECRET: &str = "luna://com.webos.ls2.test.native/test/secret";
const ECHO: &str = r#"{ "word" : "Luna" }"#;
const FIRST: &str = r#"{"value":1}"#;
const SECOND: &str = r#"{"value":2}"#;
const SIGNAL: &str = r#"{"value":42}"#;

fn main() -> Result<(), Box<dyn Error>> {
    async_io::block_on(run())
}

async fn run() -> Result<(), Box<dyn Error>> {
    let action = std::env::args().nth(1).unwrap_or_default();
    match action.as_str() {
        "active" => active().await?,
        "serve" => serve().await?,
        "client" => client().await?,
        "register" => {
            let bus = builder().connect().await?;
            println!("PROFILE {:?}", bus.protocol_profile());
            bus.close().await;
        }
        "watch" => watch(false).await?,
        "disconnect" => watch(true).await?,
        "emit" => emit().await?,
        _ => {
            return Err(io::Error::other(
                "usage: tv_smoke active|serve|client|register|watch|emit|disconnect",
            )
            .into());
        }
    }
    Ok(())
}

fn builder() -> ConnectionBuilder {
    Connection::builder().timeout(Duration::from_secs(5))
}

async fn active() -> Result<(), Box<dyn Error>> {
    let bus = match builder().connect().await {
        Ok(bus) => bus,
        Err(error) if error.kind() == ErrorKind::PermissionDenied => {
            println!("ACTIVE_DENIED");
            return Ok(());
        }
        Err(error) => return Err(error.into()),
    };
    let raw = match bus
        .call_raw("luna://com.palm.systemservice/time/getSystemTime", "{}")
        .await
    {
        Ok(raw) => raw,
        Err(error) if error.kind() == ErrorKind::PermissionDenied => {
            println!("ACTIVE_DENIED");
            bus.close().await;
            return Ok(());
        }
        Err(error) => return Err(error.into()),
    };
    let reply = json::parse(&raw)?;
    check(
        reply["returnValue"].as_bool() == Some(true),
        "TV clock call failed",
    )?;
    check(reply["utc"].as_i64().is_some(), "TV clock reply lacks utc")?;
    println!("ACTIVE_OK");
    bus.close().await;
    Ok(())
}

async fn serve() -> Result<(), Box<dyn Error>> {
    struct Cancellation;
    impl Drop for Cancellation {
        fn drop(&mut self) {
            println!("CANCELLED");
        }
    }

    let mut router = Router::new();
    router.route("/test/echo", |request| async move {
        Ok(Reply::one(request.into_payload()))
    })?;
    router.route("/test/updates", |_| async {
        let cancellation = Cancellation;
        let pending = stream::poll_fn(move |_| {
            let _keep_alive = &cancellation;
            std::task::Poll::Pending
        });
        let updates = stream::iter([Ok(FIRST.to_owned()), Ok(SECOND.to_owned())]);
        Ok(Reply::stream(updates.chain(pending)))
    })?;
    router.route("/test/secret", |_| async {
        println!("SECRET_DISPATCHED");
        Ok(Reply::one(r#"{"returnValue":true}"#))
    })?;
    let _bus = builder().register(RUST_NAME, router).await?;
    println!("READY");
    future::pending::<()>().await;
    Ok(())
}

async fn client() -> Result<(), Box<dyn Error>> {
    let bus = builder().register(RUST_NAME, Router::new()).await?;
    let echo = bus.call_raw(NATIVE_ECHO, ECHO).await?;
    check(echo == ECHO, "native echo changed raw payload")?;
    #[cfg(feature = "serde")]
    {
        let request = serde_json::json!({"word": "Luna"});
        let reply: serde_json::Value = bus.call_json(NATIVE_ECHO, &request).await?;
        check(reply == request, "native echo changed typed JSON")?;
    }
    let mut updates = bus
        .subscribe_raw(NATIVE_UPDATES, r#"{"subscribe":true}"#)
        .await?;
    for expected in [FIRST, SECOND] {
        let actual = updates.next().await.transpose()?;
        check(
            actual.as_deref() == Some(expected),
            "unexpected native update",
        )?;
    }
    updates.cancel().await?;
    if bus.protocol_profile() == ProtocolProfile::LocalListener {
        bus.close().await;
        return Ok(());
    }
    match bus.call_raw(NATIVE_SECRET, "{}").await {
        Err(error) if error.kind() == ErrorKind::PermissionDenied => {}
        result => {
            return Err(io::Error::other(format!(
                "expected native method denial, received {result:?}"
            ))
            .into());
        }
    }
    bus.close().await;
    Ok(())
}

async fn watch(disconnect: bool) -> Result<(), Box<dyn Error>> {
    let bus = builder().register(RUST_NAME, Router::new()).await?;
    let mut signals = bus
        .watch_signal(SignalFilter::exact("/test", "changed")?)
        .await?;
    println!("READY");
    if disconnect {
        return check(
            matches!(signals.next().await, Some(Err(error)) if error.kind() == ErrorKind::Disconnected),
            "hub disconnect did not terminate the Rust signal stream",
        ).map_err(Into::into);
    }
    let received = signals
        .next()
        .await
        .transpose()?
        .ok_or_else(|| io::Error::other("signal stream closed"))?;
    check(received.payload() == SIGNAL, "unexpected native signal")?;
    signals.cancel().await?;
    bus.close().await;
    Ok(())
}

async fn emit() -> Result<(), Box<dyn Error>> {
    let bus = builder().register(RUST_NAME, Router::new()).await?;
    bus.emit_raw("/test", "changed", SIGNAL).await?;
    bus.close().await;
    Ok(())
}

fn check(condition: bool, failure: &str) -> io::Result<()> {
    if condition {
        Ok(())
    } else {
        Err(io::Error::other(failure))
    }
}
