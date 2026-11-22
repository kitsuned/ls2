//! The little-endian, 32-bit Luna Service 2 wire format used by webOS TV.

mod messages;
mod socket;

use std::os::fd::OwnedFd;

pub(crate) use messages::{
    append_category, cancel, hub_method_call, method_call, method_error, parse_cancel,
    parse_method_call, parse_reply, parse_signal, peer_offer, query_name, registration, reply,
    reply_token, request_name, signal, signal_match, validate_reply_payload,
};
pub(crate) use socket::Socket;

// Protocol version 1 uses a native C header on the TV's ARM32 userspace.
const HEADER_LEN: usize = 16;
pub(crate) const MAX_BODY_LEN: usize = 10 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    MethodCall,
    HubMethodCall,
    Reply,
    ReplyWithFd,
    Signal,
    RequestName,
    RequestNameReply,
    QueryName,
    QueryNameReply,
    Shutdown,
    ServiceDownSignal,
    ServiceUpSignal,
    SignalRegister,
    SignalUnregister,
    Error,
    ErrorUnknownMethod,
    CancelMethodCall,
    MonitorConnected,
    MonitorNotConnected,
    MonitorAcceptClient,
    AppendCategory,
    Unknown(u32),
}

impl Kind {
    fn from_wire(tag: u32) -> Self {
        match tag {
            0 => Self::MethodCall,
            1 => Self::HubMethodCall,
            2 => Self::Reply,
            3 => Self::ReplyWithFd,
            4 => Self::Signal,
            5 => Self::RequestName,
            6 => Self::RequestNameReply,
            7 => Self::QueryName,
            8 => Self::QueryNameReply,
            9 => Self::Shutdown,
            10 => Self::ServiceDownSignal,
            11 => Self::ServiceUpSignal,
            12 => Self::SignalRegister,
            13 => Self::SignalUnregister,
            14 => Self::Error,
            15 => Self::ErrorUnknownMethod,
            16 => Self::CancelMethodCall,
            18 => Self::MonitorConnected,
            19 => Self::MonitorNotConnected,
            20 => Self::MonitorAcceptClient,
            28 => Self::AppendCategory,
            tag => Self::Unknown(tag),
        }
    }

    fn to_wire(self) -> u32 {
        match self {
            Self::MethodCall => 0,
            Self::HubMethodCall => 1,
            Self::Reply => 2,
            Self::ReplyWithFd => 3,
            Self::Signal => 4,
            Self::RequestName => 5,
            Self::RequestNameReply => 6,
            Self::QueryName => 7,
            Self::QueryNameReply => 8,
            Self::Shutdown => 9,
            Self::ServiceDownSignal => 10,
            Self::ServiceUpSignal => 11,
            Self::SignalRegister => 12,
            Self::SignalUnregister => 13,
            Self::Error => 14,
            Self::ErrorUnknownMethod => 15,
            Self::CancelMethodCall => 16,
            Self::MonitorConnected => 18,
            Self::MonitorNotConnected => 19,
            Self::MonitorAcceptClient => 20,
            Self::AppendCategory => 28,
            Self::Unknown(tag) => tag,
        }
    }

    fn has_fd_slot(self) -> bool {
        matches!(
            self,
            Self::ReplyWithFd
                | Self::QueryNameReply
                | Self::MonitorConnected
                | Self::MonitorAcceptClient
        )
    }
}

#[derive(Debug)]
pub(crate) struct Frame {
    pub(crate) kind: Kind,
    pub(crate) token: u32,
    pub(crate) public: bool,
    pub(crate) body: Vec<u8>,
    pub(crate) fd: Option<OwnedFd>,
}

impl Frame {
    pub(crate) fn new(kind: Kind, token: u32, public: bool, body: Vec<u8>) -> Self {
        Self {
            kind,
            token,
            public,
            body,
            fd: None,
        }
    }
}
