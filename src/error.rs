use std::{error, fmt, io, sync::Arc};

/// A class of failure that callers can handle programmatically.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum ErrorKind {
    /// An address, name, or payload is invalid.
    InvalidInput,
    /// The hub or a peer rejected the operation.
    Rejected,
    /// The hub or a peer denied permission for the operation.
    PermissionDenied,
    /// The operation exceeded its configured deadline.
    Timeout,
    /// The hub, peer, or local connection closed.
    Disconnected,
    /// A message is incompatible with the Luna wire protocol.
    Protocol,
    /// A socket operation failed.
    Io,
    /// The outstanding-work or stream buffer limit was reached.
    Overloaded,
    /// A JSON payload could not be serialized or deserialized.
    Json,
}

/// A transport, bus, or payload error from Luna IPC.
#[derive(Clone, Debug)]
pub struct Error {
    kind: ErrorKind,
    message: Arc<str>,
    code: Option<i32>,
    source: Option<Arc<dyn error::Error + Send + Sync>>,
}

impl Error {
    /// Returns the class of this error.
    pub fn kind(&self) -> ErrorKind {
        self.kind
    }

    /// Returns the numeric rejection code when supplied by the bus.
    pub fn bus_code(&self) -> Option<i32> {
        self.code
    }

    pub(crate) fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: Arc::from(message.into()),
            code: None,
            source: None,
        }
    }

    pub(crate) fn invalid(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::InvalidInput, message)
    }

    pub(crate) fn protocol(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Protocol, message)
    }

    pub(crate) fn disconnected(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Disconnected, message)
    }

    pub(crate) fn bus(kind: ErrorKind, code: Option<i32>, message: impl Into<String>) -> Self {
        Self {
            code,
            ..Self::new(kind, message)
        }
    }

    pub(crate) fn io(operation: &str, source: io::Error) -> Self {
        let kind = match source.kind() {
            io::ErrorKind::InvalidData => ErrorKind::Protocol,
            io::ErrorKind::PermissionDenied => ErrorKind::PermissionDenied,
            io::ErrorKind::UnexpectedEof
            | io::ErrorKind::BrokenPipe
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::ConnectionReset => ErrorKind::Disconnected,
            _ => ErrorKind::Io,
        };
        Self {
            source: Some(Arc::new(source)),
            ..Self::new(kind, operation)
        }
    }

    #[cfg(feature = "serde")]
    pub(crate) fn json(source: serde_json::Error) -> Self {
        Self {
            source: Some(Arc::new(source)),
            ..Self::new(ErrorKind::Json, "convert Luna JSON payload")
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)?;
        if let Some(code) = self.code {
            write!(f, " ({code})")?;
        }
        if let Some(source) = &self.source {
            write!(f, ": {source}")?;
        }
        Ok(())
    }
}

impl error::Error for Error {
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        self.source.as_deref().map(|source| source as _)
    }
}
