use std::io;

use super::Kind::{self, *};
use crate::ProtocolProfile;

impl ProtocolProfile {
    fn kinds(self) -> &'static [Kind] {
        match self {
            Self::LocalListener => &LOCAL_LISTENER_IDS,
            Self::InitiatorInfo => &INITIATOR_INFO_IDS,
            Self::TypedReplies => &NODE_UP_TYPED_IDS[..37],
            Self::NodeUpWithClientConnected | Self::TrustBeforeFlags => &NODE_UP_TYPED_IDS,
            Self::PermanentSubscriptions => &PERMANENT_SUBSCRIPTION_IDS[..38],
            Self::IntentResolution | Self::LengthPrefixedReplies => &PERMANENT_SUBSCRIPTION_IDS,
        }
    }

    pub(super) fn decode_kind(self, raw: u32) -> io::Result<Kind> {
        self.kinds()
            .get(raw as usize)
            .copied()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "unknown LS2 message type"))
    }

    pub(super) fn encode_kind(self, kind: Kind) -> io::Result<u32> {
        self.kinds()
            .iter()
            .position(|entry| *entry == kind && kind != Ignored)
            .map(|raw| raw as u32)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "message type is unavailable on this Luna protocol profile",
                )
            })
    }

    pub(crate) fn descriptor_tag_mask(self) -> u64 {
        self.kinds()
            .iter()
            .enumerate()
            .filter(|(_, kind)| self.has_descriptor(**kind))
            .fold(0, |mask, (tag, _)| mask | (1 << tag))
    }

    pub(super) fn has_bus_flag(self) -> bool {
        self != Self::LocalListener
    }

    pub(super) fn has_descriptor(self, kind: Kind) -> bool {
        match kind {
            RequestNameReply => self == Self::LocalListener,
            ReplyWithFd | QueryNameReply | MonitorConnected | MonitorAcceptClient | Initiator => {
                true
            }
            _ => false,
        }
    }
}

// Indexed by the message number used on the wire. Ignored slots have no FD
// trailer. Lists of descriptor-bearing types were verified in the stock
// library's transport FD classifier.
#[rustfmt::skip]
const LOCAL_LISTENER_IDS: [Kind; 32] = [
    /*  0 */ MethodCall, Reply, Signal, NodeUp,
    /*  4 */ RequestName, Ignored, RequestNameReply, Ignored,
    /*  8 */ QueryName, QueryNameReply, Shutdown, ServiceDownSignal,
    /* 12 */ ServiceUpSignal, SignalRegister, SignalUnregister, Error,
    /* 16 */ ErrorUnknownMethod, CancelMethodCall, ClientInfo, Ignored,
    /* 20 */ MonitorConnected, MonitorNotConnected, Ignored, Ignored,
    /* 24 */ Ignored, Ignored, Ignored, Ignored,
    /* 28 */ Ignored, AppendCategory, Ignored, Ignored,
];
#[rustfmt::skip]
const INITIATOR_INFO_IDS: [Kind; 31] = [
    /*  0 */ MethodCall, Reply, Signal, NodeUp,
    /*  4 */ RequestName, RequestNameReply, QueryName, QueryNameReply,
    /*  8 */ Shutdown, ServiceDownSignal, ServiceUpSignal, SignalRegister,
    /* 12 */ SignalUnregister, Error, ErrorUnknownMethod, CancelMethodCall,
    /* 16 */ ClientInfo, Ignored, MonitorConnected, MonitorNotConnected,
    /* 20 */ Ignored, Ignored, Ignored, Ignored,
    /* 24 */ Ignored, Ignored, Ignored, AppendCategory,
    /* 28 */ Ignored, Ignored, Initiator,
];
#[rustfmt::skip]
const NODE_UP_TYPED_IDS: [Kind; 38] = [
    /*  0 */ MethodCall, HubMethodCall, Reply, ReplyWithFd,
    /*  4 */ Signal, NodeUp, RequestName, RequestNameReply,
    /*  8 */ QueryName, QueryNameReply, Shutdown, ServiceDownSignal,
    /* 12 */ ServiceUpSignal, SignalRegister, SignalUnregister, Error,
    /* 16 */ ErrorUnknownMethod, CancelMethodCall, ClientInfo, Ignored,
    /* 20 */ MonitorConnected, MonitorNotConnected, MonitorAcceptClient, Ignored,
    /* 24 */ Ignored, Ignored, Ignored, Ignored,
    /* 28 */ Ignored, Ignored, AppendCategory, Ignored,
    /* 32 */ Ignored, Ignored, Ignored, Ignored,
    /* 36 */ Ignored, ClientConnected,
];
#[rustfmt::skip]
const PERMANENT_SUBSCRIPTION_IDS: [Kind; 40] = [
    /*  0 */ MethodCall, HubMethodCall, Reply, ReplyWithFd,
    /*  4 */ Signal, RequestName, RequestNameReply, QueryName,
    /*  8 */ QueryNameReply, Shutdown, ServiceDownSignal, ServiceUpSignal,
    /* 12 */ SignalRegister, SignalUnregister, Error, ErrorUnknownMethod,
    /* 16 */ CancelMethodCall, Ignored, MonitorConnected, MonitorNotConnected,
    /* 20 */ MonitorAcceptClient, Ignored, Ignored, Ignored,
    /* 24 */ Ignored, Ignored, Ignored, Ignored,
    /* 28 */ AppendCategory, Ignored, Ignored, Ignored,
    /* 32 */ Ignored, Ignored, Ignored, ClientConnected,
    /* 36 */ Ignored, Ignored, Ignored, Ignored,
];
