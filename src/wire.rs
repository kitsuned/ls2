//! The little-endian, 32-bit Luna Service 2 wire format used by webOS TV.

mod messages;
mod numbering;
mod socket;

use std::os::fd::OwnedFd;

pub(crate) use messages::{
    append_category, cancel, client_info, hub_method_call, method_call, parse_cancel,
    parse_method_call, parse_reply, parse_signal, peer_offer, query_name, registration, reply,
    reply_token, request_name, signal, signal_match, validate_reply_payload,
};
pub(crate) use socket::{Listener, Socket};

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
    NodeUp,
    ClientInfo,
    Initiator,
    ClientConnected,
    Ignored,
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
