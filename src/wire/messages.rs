use std::io;
use std::str;

use super::{Frame, Kind, MAX_BODY_LEN};

const PROTOCOL_VERSION: i32 = 1;
const ARG_END: u32 = 0;
const ARG_STRING: u32 = 1;
const ARG_I32: u32 = 2;
const ARG_HEADER_LEN: usize = 8;
const STR_HEADER_LEN: usize = 12;
const REPLY_PADDING: [u8; 3] = [0; 3];
const JSON_PAYLOAD_TYPE: &str = "json";
const REPLY_PREFIX_LEN: usize =
    2 * size_of::<u32>() + JSON_PAYLOAD_TYPE.len() + 1 + REPLY_PADDING.len();
const CANCEL_CATEGORY: &str = "/com/palm/luna/private";
const CANCEL_METHOD: &str = "cancel";

pub(crate) struct Registration<'a> {
    pub(crate) provided_groups: Option<&'a str>,
    pub(crate) provided_trust: Option<&'a str>,
    pub(crate) flags: i32,
}

pub(crate) struct PeerOffer<'a> {
    pub(crate) code: i32,
    pub(crate) name: Option<&'a str>,
    pub(crate) unique_name: Option<&'a str>,
    pub(crate) permissions: i32,
    pub(crate) groups: Option<&'a str>,
    pub(crate) trust: Option<&'a str>,
}

pub(crate) struct MethodCall<'a> {
    pub(crate) category: &'a str,
    pub(crate) method: &'a str,
    pub(crate) payload: &'a str,
}

pub(crate) struct SignalMessage<'a> {
    pub(crate) category: &'a str,
    pub(crate) method: &'a str,
    pub(crate) payload: &'a str,
}

pub(crate) fn request_name(token: u32, name: Option<&str>) -> io::Result<Frame> {
    let mut args = Arguments::default();
    args.i32(PROTOCOL_VERSION)?;
    args.string(name)?;
    args.string(None)?;
    args.frame(Kind::RequestName, token)
}

pub(crate) fn registration(frame: &Frame) -> io::Result<Result<Registration<'_>, i32>> {
    if frame.kind != Kind::RequestNameReply {
        return Err(invalid_data("expected a Luna registration reply"));
    }
    let mut args = ArgReader::new(&frame.body);
    let code = args.i32()?;
    if code != 0 {
        return Ok(Err(code));
    }

    args.i32()?; // Privileged flag.
    args.i32()?; // Proxy flag.
    if args.string()?.is_none_or(str::is_empty) {
        return Err(invalid_data("registration reply is missing a unique name"));
    }

    let provided_groups = args.string()?;
    let flags = args.i32()?;
    let provided_trust = args.string()?;
    args.string()?; // Our trust level.
    Ok(Ok(Registration {
        provided_groups,
        provided_trust,
        flags,
    }))
}

pub(crate) fn query_name(token: u32, name: &str) -> io::Result<Frame> {
    let mut args = Arguments::default();
    args.string(Some(name))?;
    args.string(None)?;
    args.frame(Kind::QueryName, token)
}

pub(crate) fn peer_offer(frame: &Frame) -> io::Result<PeerOffer<'_>> {
    let mut args = ArgReader::new(&frame.body);
    let code = args.i32()?;
    let name = args.string()?;
    let unique_name = args.string()?;
    args.i32()?; // Whether the hub launched the service on demand.
    let (groups, permissions, trust) = if code == 0 {
        args.string()?; // The peer's application ID.
        let groups = args.string()?;
        let permissions = args.i32()?;
        (groups, permissions, args.string()?)
    } else {
        // A failed query ends after the dynamic flag with native allocation padding.
        (None, 0, None)
    };
    Ok(PeerOffer {
        code,
        name,
        unique_name,
        permissions,
        groups,
        trust,
    })
}

pub(crate) fn append_category<'a>(
    token: u32,
    category: &str,
    methods: impl IntoIterator<Item = &'a str>,
) -> io::Result<Frame> {
    let mut args = Arguments::default();
    args.string(Some(category))?;
    args.i32(0)?; // Category flags for regular method registration.
    for method in methods {
        args.string(Some(method))?;
    }
    Ok(Frame::new(Kind::AppendCategory, token, false, args.0))
}

pub(crate) fn hub_method_call(token: u32, method: &str, payload: &str) -> io::Result<Frame> {
    let mut args = Arguments::default();
    args.string(Some(method))?;
    args.string(Some(payload))?;
    args.frame(Kind::HubMethodCall, token)
}

pub(crate) fn method_call(
    token: u32,
    category: &str,
    method: &str,
    payload: &str,
) -> io::Result<Frame> {
    string_frame(
        Kind::MethodCall,
        token,
        &[category, method, payload, "", ""],
    )
}

pub(crate) fn parse_method_call(frame: &Frame) -> io::Result<MethodCall<'_>> {
    let mut bytes = frame.body.as_slice();
    let category = read_cstr(&mut bytes)?;
    let method = read_cstr(&mut bytes)?;
    let payload = read_cstr(&mut bytes)?;
    // The per-call application ID is self-reported. Only trust the hub offer's metadata.
    read_cstr(&mut bytes)?;
    read_cstr(&mut bytes)?;
    end_of_body(bytes)?;
    Ok(MethodCall {
        category,
        method,
        payload,
    })
}

pub(crate) fn reply(token: u32, reply_to: u32, payload: &str) -> io::Result<Frame> {
    reply_frame(Kind::Reply, token, reply_to, payload)
}

pub(crate) fn method_error(
    kind: Kind,
    token: u32,
    reply_to: u32,
    payload: &str,
) -> io::Result<Frame> {
    reply_frame(kind, token, reply_to, payload)
}

fn reply_frame(kind: Kind, token: u32, reply_to: u32, payload: &str) -> io::Result<Frame> {
    validate_reply_payload(payload)?;
    let payload_len = payload.len() + 1;
    let size = wire_len(payload_len)?;
    let mut body = Vec::with_capacity(REPLY_PREFIX_LEN + payload_len);
    body.extend_from_slice(&reply_to.to_le_bytes());
    body.extend_from_slice(JSON_PAYLOAD_TYPE.as_bytes());
    body.push(0);
    body.extend_from_slice(&size.to_le_bytes());
    body.extend_from_slice(&REPLY_PADDING);
    body.extend_from_slice(payload.as_bytes());
    body.push(0);
    Ok(Frame::new(kind, token, false, body))
}

pub(crate) fn validate_reply_payload(payload: &str) -> io::Result<()> {
    ensure_room(REPLY_PREFIX_LEN, payload.len().saturating_add(1))?;
    validate_cstr(payload)
}

pub(crate) fn reply_token(frame: &Frame) -> io::Result<u32> {
    read_u32(&mut frame.body.as_slice())
}

pub(crate) fn parse_reply(frame: &Frame) -> io::Result<(u32, &str)> {
    let mut bytes = frame.body.as_slice();
    let reply_to = read_u32(&mut bytes)?;
    if read_cstr(&mut bytes)? != JSON_PAYLOAD_TYPE {
        return Err(invalid_data("unsupported reply payload type"));
    }
    let size = read_u32(&mut bytes)? as usize;
    take_array::<{ REPLY_PADDING.len() }>(&mut bytes)?;
    if bytes.len() != size {
        return Err(invalid_data("invalid reply payload length"));
    }
    let payload = read_cstr(&mut bytes)?;
    end_of_body(bytes)?;
    Ok((reply_to, payload))
}

pub(crate) fn cancel(token: u32, cancelled: u32) -> io::Result<Frame> {
    // The reference ARM32 client represents this token as a signed JSON integer.
    let cancelled = i32::from_le_bytes(cancelled.to_le_bytes());
    let payload = format!(r#"{{"token":{cancelled}}}"#);
    string_frame(
        Kind::CancelMethodCall,
        token,
        &[CANCEL_CATEGORY, CANCEL_METHOD, &payload],
    )
}

pub(crate) fn parse_cancel(frame: &Frame) -> io::Result<u32> {
    let mut bytes = frame.body.as_slice();
    let category = read_cstr(&mut bytes)?;
    let method = read_cstr(&mut bytes)?;
    if category != CANCEL_CATEGORY || method != CANCEL_METHOD {
        return Err(invalid_data("invalid cancellation path"));
    }
    let payload = read_cstr(&mut bytes)?;
    end_of_body(bytes)?;
    let payload =
        json::parse(payload).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    payload["token"]
        .as_u32()
        .or_else(|| {
            payload["token"]
                .as_i32()
                .map(|value| u32::from_le_bytes(value.to_le_bytes()))
        })
        .ok_or_else(|| invalid_data("cancellation is missing a call token"))
}

pub(crate) fn signal_match(
    kind: Kind,
    token: u32,
    category: &str,
    method: Option<&str>,
) -> io::Result<Frame> {
    string_frame(kind, token, &[category, method.unwrap_or_default()])
}

pub(crate) fn signal(token: u32, category: &str, method: &str, payload: &str) -> io::Result<Frame> {
    string_frame(Kind::Signal, token, &[category, method, payload])
}

pub(crate) fn parse_signal(frame: &Frame) -> io::Result<SignalMessage<'_>> {
    let mut bytes = frame.body.as_slice();
    let category = read_cstr(&mut bytes)?;
    let method = read_cstr(&mut bytes)?;
    let payload = read_cstr(&mut bytes)?;
    end_of_body(bytes)?;
    Ok(SignalMessage {
        category,
        method,
        payload,
    })
}

fn string_frame(kind: Kind, token: u32, values: &[&str]) -> io::Result<Frame> {
    let mut body = Vec::new();
    for value in values {
        append_cstr(&mut body, value)?;
    }
    Ok(Frame::new(kind, token, false, body))
}

#[derive(Default)]
struct Arguments(Vec<u8>);

impl Arguments {
    fn i32(&mut self, value: i32) -> io::Result<()> {
        ensure_room(self.0.len(), ARG_HEADER_LEN + size_of::<i32>())?;
        self.u32(ARG_I32);
        self.u32(4);
        self.0.extend_from_slice(&value.to_le_bytes());
        Ok(())
    }

    fn string(&mut self, value: Option<&str>) -> io::Result<()> {
        let string_len = value.map_or(0, |value| value.len().saturating_add(1));
        ensure_room(self.0.len(), string_len.saturating_add(STR_HEADER_LEN))?;
        let padded_len = string_len.next_multiple_of(size_of::<u32>());
        ensure_room(self.0.len(), padded_len + STR_HEADER_LEN)?;

        self.u32(ARG_STRING);
        self.u32(wire_len(padded_len)?);
        self.u32(wire_len(string_len)?);
        let end = self.0.len() + padded_len;
        if let Some(value) = value {
            append_cstr(&mut self.0, value)?;
        }
        self.0.resize(end, 0);
        Ok(())
    }

    fn u32(&mut self, word: u32) {
        self.0.extend_from_slice(&word.to_le_bytes());
    }

    fn frame(mut self, kind: Kind, token: u32) -> io::Result<Frame> {
        ensure_room(self.0.len(), ARG_HEADER_LEN)?;
        self.u32(ARG_END);
        self.u32(0);
        Ok(Frame::new(kind, token, false, self.0))
    }
}

struct ArgReader<'a>(&'a [u8]);

impl<'a> ArgReader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self(bytes)
    }

    fn i32(&mut self) -> io::Result<i32> {
        let tag = read_u32(&mut self.0)?;
        let len = read_u32(&mut self.0)?;
        if tag != ARG_I32 || len != 4 {
            return Err(invalid_data("expected a 32-bit integer argument"));
        }
        Ok(i32::from_le_bytes(take_array(&mut self.0)?))
    }

    fn string(&mut self) -> io::Result<Option<&'a str>> {
        let tag = read_u32(&mut self.0)?;
        let padded_len = read_u32(&mut self.0)? as usize;
        if tag != ARG_STRING {
            return Err(invalid_data("expected a string argument"));
        }
        let len = read_u32(&mut self.0)? as usize;
        let expected = len.checked_next_multiple_of(size_of::<u32>());
        if expected != Some(padded_len) {
            return Err(invalid_data("invalid padded string argument length"));
        }
        let (value, tail) = self
            .0
            .split_at_checked(padded_len)
            .ok_or_else(|| invalid_data("truncated string argument"))?;
        self.0 = tail;
        if len == 0 {
            return Ok(None);
        }
        let mut bytes = &value[..len];
        let string = read_cstr(&mut bytes)?;
        end_of_body(bytes)?;
        Ok(Some(string))
    }
}

fn take_array<const N: usize>(bytes: &mut &[u8]) -> io::Result<[u8; N]> {
    let (prefix, tail) = bytes
        .split_first_chunk::<N>()
        .ok_or_else(|| invalid_data("truncated message field"))?;
    *bytes = tail;
    Ok(*prefix)
}

fn read_u32(bytes: &mut &[u8]) -> io::Result<u32> {
    Ok(u32::from_le_bytes(take_array(bytes)?))
}

fn read_cstr<'a>(bytes: &mut &'a [u8]) -> io::Result<&'a str> {
    let Some(end) = bytes.iter().position(|&byte| byte == 0) else {
        return Err(invalid_data("missing string terminator"));
    };
    let value = str::from_utf8(&bytes[..end])
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    *bytes = &bytes[end + 1..];
    Ok(value)
}

fn append_cstr(bytes: &mut Vec<u8>, value: &str) -> io::Result<()> {
    ensure_room(bytes.len(), value.len().saturating_add(1))?;
    validate_cstr(value)?;
    bytes.extend_from_slice(value.as_bytes());
    bytes.push(0);
    Ok(())
}

fn validate_cstr(value: &str) -> io::Result<()> {
    if value.as_bytes().contains(&0) {
        Err(io::Error::new(io::ErrorKind::InvalidInput, "embedded NUL"))
    } else {
        Ok(())
    }
}

fn end_of_body(bytes: &[u8]) -> io::Result<()> {
    if bytes.is_empty() {
        Ok(())
    } else {
        Err(invalid_data("unexpected trailing message bytes"))
    }
}

fn ensure_room(used: usize, extra: usize) -> io::Result<()> {
    if extra <= MAX_BODY_LEN.saturating_sub(used) {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "message body exceeds the LS2 frame limit",
        ))
    }
}

fn wire_len(len: usize) -> io::Result<u32> {
    u32::try_from(len).map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))
}

fn invalid_data(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_name_uses_native_alignment_and_null_strings() {
        let request = request_name(7, Some("a.b")).unwrap();
        let expected = [
            2, 0, 0, 0, 4, 0, 0, 0, 1, 0, 0, 0, // Protocol version.
            1, 0, 0, 0, 4, 0, 0, 0, 4, 0, 0, 0, b'a', b'.', b'b', 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, // Anonymous application ID.
            0, 0, 0, 0, 0, 0, 0, 0, // End marker.
        ];
        assert_eq!(request.body, expected);
    }

    #[test]
    fn name_lookup_failure_can_end_after_the_dynamic_flag() {
        let mut args = Arguments::default();
        args.i32(-4).unwrap();
        args.string(Some("com.example.missing")).unwrap();
        args.string(None).unwrap();
        args.i32(0).unwrap();
        let frame = Frame::new(Kind::QueryNameReply, 4, false, args.0);
        let offer = peer_offer(&frame).unwrap();
        assert_eq!(offer.code, -4);
        assert_eq!(offer.name, Some("com.example.missing"));
        assert_eq!(offer.unique_name, None);
    }

    #[test]
    fn tv_registration_places_flags_before_trust_rules() {
        let mut args = Arguments::default();
        for value in [0, 0, 0] {
            args.i32(value).unwrap();
        }
        args.string(Some("tFX0oVEt")).unwrap();
        args.string(Some(r#"[{"category":"/test/echo","groups":["echo"]}]"#))
            .unwrap();
        args.i32(0).unwrap();
        args.string(Some(r#"[{"group":"echo","trust":[""]}]"#))
            .unwrap();
        args.string(Some("public")).unwrap();
        let frame = args.frame(Kind::RequestNameReply, 1).unwrap();
        let accepted = registration(&frame).unwrap().unwrap();
        assert_eq!(accepted.flags, 0);
        assert_eq!(
            accepted.provided_groups,
            Some(r#"[{"category":"/test/echo","groups":["echo"]}]"#),
        );
        assert_eq!(
            accepted.provided_trust,
            Some(r#"[{"group":"echo","trust":[""]}]"#),
        );
    }

    #[test]
    fn tv_peer_offer_decodes_trust_before_auxiliary_metadata() {
        let mut args = Arguments::default();
        args.i32(0).unwrap();
        args.string(Some("com.palm.systemservice")).unwrap();
        args.string(Some("5pLb2jEk")).unwrap();
        args.i32(0).unwrap();
        args.string(None).unwrap();
        args.string(Some(r#"["time.query"]"#)).unwrap();
        args.i32(2).unwrap();
        args.string(Some("platform")).unwrap();
        args.string(None).unwrap();
        args.string(Some("[]")).unwrap();
        let frame = args.frame(Kind::QueryNameReply, 1).unwrap();
        let offer = peer_offer(&frame).unwrap();
        assert_eq!(offer.permissions, 2);
        assert_eq!(offer.groups, Some(r#"["time.query"]"#));
        assert_eq!(offer.trust, Some("platform"));
    }

    #[test]
    fn malformed_typed_string_cannot_consume_the_following_argument() {
        let mut args = Arguments::default();
        args.string(Some("x")).unwrap();
        args.i32(3).unwrap();
        args.0[8] = 100;
        assert_eq!(
            ArgReader::new(&args.0).string().unwrap_err().kind(),
            io::ErrorKind::InvalidData,
        );
    }

    #[test]
    fn raw_payload_preserves_whitespace_and_unicode() {
        let payload = " {\"message\": \"Grüße\"} \n";
        let call = method_call(1, "/", "test", payload).unwrap();
        assert_eq!(parse_method_call(&call).unwrap().payload, payload);
        let reply = reply(2, 1, payload).unwrap();
        assert_eq!(parse_reply(&reply).unwrap(), (1, payload));
    }

    #[test]
    fn tv_reply_contains_explicit_payload_length_and_alignment() {
        let payload = r#"{"returnValue":true}"#;
        let body = b"\x02\0\0\0json\0\x15\0\0\0\0\0\0{\"returnValue\":true}\0";
        let frame = reply(5, 2, payload).unwrap();
        assert_eq!(&frame.body, body);
        assert_eq!(parse_reply(&frame).unwrap(), (2, payload));
    }

    #[test]
    fn tv_method_call_has_two_fields_after_the_payload() {
        let frame = method_call(2, "/time", "getSystemTime", "{}").unwrap();
        assert_eq!(&frame.body, b"/time\0getSystemTime\0{}\0\0\0");
        let call = parse_method_call(&frame).unwrap();
        assert_eq!(call.category, "/time");
        assert_eq!(call.method, "getSystemTime");
        assert_eq!(call.payload, "{}");
    }

    #[test]
    fn tv_append_category_has_flags_and_no_end_marker() {
        let frame = append_category(2, "/test", ["echo", "updates", "secret"]).unwrap();
        assert_eq!(frame.kind.to_wire(), 28);
        assert_eq!(frame.body.len(), 92);
        let mut args = ArgReader::new(&frame.body);
        assert_eq!(args.string().unwrap(), Some("/test"));
        assert_eq!(args.i32().unwrap(), 0);
        assert_eq!(args.string().unwrap(), Some("echo"));
        assert_eq!(args.string().unwrap(), Some("updates"));
        assert_eq!(args.string().unwrap(), Some("secret"));
        assert!(args.0.is_empty());
    }

    #[test]
    fn embedded_nul_is_rejected() {
        assert_eq!(
            method_call(1, "/", "test", "{}\0{}").unwrap_err().kind(),
            io::ErrorKind::InvalidInput,
        );
    }

    #[test]
    fn cancel_token_keeps_all_native_bits() {
        let frame = cancel(3, u32::MAX).unwrap();
        assert_eq!(parse_cancel(&frame).unwrap(), u32::MAX);
    }
}
