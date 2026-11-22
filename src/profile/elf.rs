use std::collections::BTreeMap;

use object::{Object, ObjectSection, ObjectSymbol, SymbolKind};

use super::{ProtocolProfile, ReplyLayout, thumb};

pub(super) fn profile_for_library(elf: &object::File<'_>) -> Option<ProtocolProfile> {
    let exports: BTreeMap<_, _> = elf
        .dynamic_symbols()
        .filter(|symbol| symbol.is_definition() && symbol.kind() == SymbolKind::Text)
        .filter_map(|symbol| Some((symbol.name().ok()?, symbol)))
        .collect();
    let profile = classify(|name| exports.contains_key(name))?;

    let thumb_code = |name: &str| {
        let symbol = exports.get(name)?;
        let address = symbol.address();
        if address & 1 == 0 {
            return None;
        }
        let section = elf.section_by_index(symbol.section_index()?).ok()?;
        let bytes = section.data_range(address & !1, symbol.size()).ok()??;
        Some((bytes, address & !1))
    };
    let layout = profile.reply_layout();
    let fd_classifier = if layout == ReplyLayout::Plain {
        "_LSTransportMessageIsConnectionFdType"
    } else {
        "_LSTransportMessageIsFdType"
    };
    let (code, address) = thumb_code(fd_classifier)?;
    if thumb::descriptor_tags(code, address)? != profile.descriptor_tag_mask() {
        return None;
    }
    if layout != ReplyLayout::Plain {
        let (code, _) = thumb_code("_LSPayloadDeserialize")?;
        if thumb::payload_layout(code)? != layout {
            return None;
        }
    }
    Some(profile)
}

fn classify(has: impl Fn(&str) -> bool) -> Option<ProtocolProfile> {
    use ProtocolProfile::{
        InitiatorInfo, IntentResolution, LengthPrefixedReplies, LocalListener,
        NodeUpWithClientConnected, PermanentSubscriptions, TrustBeforeFlags, TypedReplies,
    };

    // A feature's related transport functions must agree. A partial symbol set
    // can indicate a library from a different LS2 implementation.
    let feature = |symbols: &[&str]| {
        let first = has(symbols[0]);
        symbols
            .iter()
            .all(|name| has(name) == first)
            .then_some(first)
    };
    if ![
        "_LSTransportRequestName",
        "_LSTransportQueryName",
        "_LSTransportAppendCategory",
    ]
    .into_iter()
    .all(&has)
    {
        return None;
    }

    let local_and_inet = feature(&[
        "_LSTransportRequestNameLocal",
        "_LSTransportRequestNameInet",
        "_LSTransportSendMessageNodeUp",
    ])?;
    let node_up = feature(&["_LSTransportNodeUp", "_LSTransportSendMessageClientInfo"])?;
    let client_connected = feature(&[
        "_LSTransportSendMessageClientConnected",
        "_LSTransportHandleShutdownWithSerial",
    ])?;
    let trust = feature(&[
        "LSTransportGetTrustLevelToGroups",
        "_LSTransportClientGetTrustString",
    ])?;
    let permanent_subscriptions = feature(&[
        "LSTransportSendSubscriptionAdd",
        "LSTransportSendSubscriptionRemove",
    ])?;
    let intent = feature(&["LSTransportSendResolveIntent", "LSIntentSendOneReply"])?;
    let typed_payload = feature(&[
        "_LSTransportMessageIsFdType",
        "_LSTransportSendReplyString",
        "LSTransportSendMethodToHub",
        "_LSPayloadSerialize",
        "_LSPayloadDeserialize",
    ])?;

    if typed_payload == has("_LSTransportMessageIsConnectionFdType") {
        return None;
    }
    if !typed_payload {
        if !node_up || client_connected || trust || permanent_subscriptions || intent {
            return None;
        }
        return if local_and_inet {
            has("_LSTransportSetupListenerLocalRaw").then_some(LocalListener)
        } else {
            has("_LSGetHubLocalSocketAddress").then_some(InitiatorInfo)
        };
    }
    if local_and_inet || permanent_subscriptions == node_up {
        return None;
    }

    if node_up {
        if intent {
            return None;
        }
        match (client_connected, trust) {
            (false, false) => Some(TypedReplies),
            (true, false) => Some(NodeUpWithClientConnected),
            (true, true) if has("_LSTransportQueryNameReplyGetTrustlevelString") => {
                Some(TrustBeforeFlags)
            }
            _ => None,
        }
    } else {
        if !client_connected {
            return None;
        }
        match (intent, trust) {
            (false, false) => Some(PermanentSubscriptions),
            (true, false) => Some(IntentResolution),
            (true, true) => Some(LengthPrefixedReplies),
            (false, true) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_connected_distinguishes_node_up_profiles() {
        let shared = [
            "_LSTransportRequestName",
            "_LSTransportQueryName",
            "_LSTransportAppendCategory",
            "_LSTransportNodeUp",
            "_LSTransportSendMessageClientInfo",
            "_LSTransportMessageIsFdType",
            "_LSTransportSendReplyString",
            "LSTransportSendMethodToHub",
            "_LSPayloadSerialize",
            "_LSPayloadDeserialize",
        ];
        let ready = "_LSTransportSendMessageClientConnected";
        let serial = "_LSTransportHandleShutdownWithSerial";

        assert_eq!(
            classify(|name| shared.contains(&name)),
            Some(ProtocolProfile::TypedReplies),
        );
        assert_eq!(
            classify(|name| shared.contains(&name) || name == ready),
            None
        );
        assert_eq!(
            classify(|name| shared.contains(&name) || [ready, serial].contains(&name)),
            Some(ProtocolProfile::NodeUpWithClientConnected),
        );
    }
}
