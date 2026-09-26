//! Minimal hand-rolled Google Cast V2 wire codec.
//!
//! A Cast V2 frame is a 4-byte big-endian length prefix followed by a `CastMessage`
//! protobuf (`cast_channel.proto` in the Chromium tree). Only the fields needed to
//! drive receiver and media sessions are decoded; unknown fields are skipped by wire
//! type. Keeping this codec local avoids a protobuf runtime and lets the decoder
//! enforce strict frame-size, length and UTF-8 bounds.

use std::fmt;

use serde_json::Value;

/// Maximum accepted Cast frame body (excluding the 4-byte length prefix).
pub(crate) const MAX_FRAME_BYTES: usize = 64 * 1024;
/// Maximum accepted control JSON payload.
pub(crate) const MAX_JSON_BYTES: usize = 32 * 1024;

/// Cast namespace used for virtual transport connect/close messages.
pub(crate) const NS_CONNECTION: &str = "urn:x-cast:com.google.cast.tp.connection";
/// Cast namespace used for PING/PONG liveness.
pub(crate) const NS_HEARTBEAT: &str = "urn:x-cast:com.google.cast.tp.heartbeat";
/// Cast namespace used for receiver status and application launch.
pub(crate) const NS_RECEIVER: &str = "urn:x-cast:com.google.cast.receiver";
/// Cast namespace used for media load/stop/status.
pub(crate) const NS_MEDIA: &str = "urn:x-cast:com.google.cast.media";

/// Virtual sender id for the platform receiver.
pub(crate) const RECEIVER_ID: &str = "receiver-0";
/// Default Media Receiver application id.
pub(crate) const DMR_APP_ID: &str = "CC1AD845";
/// Backdrop application id; treated as idle even though it is receiver-owned.
pub(crate) const BACKDROP_APP_ID: &str = "E8C28D3C";

const WIRE_VARINT: u8 = 0;
const WIRE_FIXED64: u8 = 1;
const WIRE_LEN: u8 = 2;
const WIRE_GROUP_START: u8 = 3;
const WIRE_GROUP_END: u8 = 4;
const WIRE_FIXED32: u8 = 5;
const MAX_FIELD_NUMBER: u64 = (1 << 29) - 1;

/// Protobuf-level failures for a Cast message body.
///
/// Messages never embed untrusted payload text, so they are safe to log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CodecError {
    /// A variable-length integer was cut off by the end of the buffer.
    UnexpectedEof(&'static str),
    /// A varint used more than ten bytes.
    VarintTooLong,
    /// A varint's final byte carried bits that do not fit in 64 bits.
    VarintOverflow,
    /// A protobuf tag used field number zero.
    FieldNumberZero,
    /// A protobuf tag used a field number above the allowed 29-bit range.
    FieldNumberTooLarge,
    /// An unknown field used an unsupported wire type.
    BadWireType(u8),
    /// An unknown field used a deprecated protobuf group.
    GroupUnsupported,
    /// A length prefix did not fit the remaining buffer.
    BadLength,
    /// A required `CastMessage` field was missing.
    MissingField(&'static str),
    /// A known field used the wrong protobuf wire type.
    BadFieldType { field: &'static str, wire: u8 },
    /// `protocol_version` was not the expected proto2 value.
    BadProtocolVersion(u64),
    /// `payload_type` was neither STRING nor BINARY.
    BadPayloadType(u64),
    /// A protobuf string field was not valid UTF-8.
    InvalidUtf8(&'static str),
    /// Both `payload_utf8` and `payload_binary` were present.
    DuplicatePayload,
    /// Neither payload field was present.
    PayloadMissing,
    /// The payload encoding did not match `payload_type`.
    PayloadTypeMismatch,
    /// The JSON control payload exceeded [`MAX_JSON_BYTES`].
    JsonTooLarge(usize),
    /// The control payload was not valid JSON.
    InvalidJson,
}

impl fmt::Display for CodecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnexpectedEof(what) => write!(f, "truncated {what}"),
            Self::VarintTooLong => write!(f, "varint longer than ten bytes"),
            Self::VarintOverflow => write!(f, "varint overflows 64 bits"),
            Self::FieldNumberZero => write!(f, "protobuf field number zero"),
            Self::FieldNumberTooLarge => write!(f, "protobuf field number out of range"),
            Self::BadWireType(wire) => write!(f, "unsupported protobuf wire type {wire}"),
            Self::GroupUnsupported => write!(f, "protobuf groups are not supported"),
            Self::BadLength => write!(f, "length-delimited field exceeds the frame"),
            Self::MissingField(field) => write!(f, "missing required field {field}"),
            Self::BadFieldType { field, wire } => {
                write!(f, "field {field} used wire type {wire}")
            }
            Self::BadProtocolVersion(value) => write!(f, "unsupported protocol version {value}"),
            Self::BadPayloadType(value) => write!(f, "unsupported payload type {value}"),
            Self::InvalidUtf8(field) => write!(f, "field {field} is not valid UTF-8"),
            Self::DuplicatePayload => write!(f, "both string and binary payloads present"),
            Self::PayloadMissing => write!(f, "message has no payload"),
            Self::PayloadTypeMismatch => write!(f, "payload encoding does not match payload type"),
            Self::JsonTooLarge(size) => write!(f, "control JSON payload is {size} bytes"),
            Self::InvalidJson => write!(f, "control payload is not valid JSON"),
        }
    }
}

impl std::error::Error for CodecError {}

/// Frame-level failures while reading the length-prefixed transport.
#[derive(Debug)]
pub(crate) enum FrameError {
    /// The underlying stream failed.
    Io(std::io::Error),
    /// A zero-length length prefix.
    EmptyFrame,
    /// The declared length exceeded [`MAX_FRAME_BYTES`].
    TooLarge(usize),
    /// The stream ended in the middle of a frame.
    Truncated,
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "cast connection error: {error}"),
            Self::EmptyFrame => write!(f, "empty cast frame"),
            Self::TooLarge(size) => {
                write!(f, "cast frame of {size} bytes exceeds the 64 KiB limit")
            }
            Self::Truncated => write!(f, "cast connection ended mid-frame"),
        }
    }
}

impl std::error::Error for FrameError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<std::io::Error> for FrameError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

/// Decoded subset of the Cast `CastMessage` protobuf.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CastMessage {
    pub(crate) protocol_version: u64,
    pub(crate) source: String,
    pub(crate) destination: String,
    pub(crate) namespace: String,
    pub(crate) payload_type: u64,
    pub(crate) payload_utf8: Option<String>,
    pub(crate) payload_binary: Option<Vec<u8>>,
}

impl CastMessage {
    /// Build a protocol version 1 / STRING payload message from a JSON value.
    pub(crate) fn json(source: &str, destination: &str, namespace: &str, payload: &Value) -> Self {
        Self {
            protocol_version: 0,
            source: source.to_owned(),
            destination: destination.to_owned(),
            namespace: namespace.to_owned(),
            payload_type: 0,
            payload_utf8: Some(payload.to_string()),
            payload_binary: None,
        }
    }

    /// Encode as a complete length-prefixed Cast frame.
    pub(crate) fn encode_frame(&self) -> Vec<u8> {
        let mut body = Vec::with_capacity(128);
        write_varint_field(&mut body, 1, self.protocol_version);
        write_bytes_field(&mut body, 2, self.source.as_bytes());
        write_bytes_field(&mut body, 3, self.destination.as_bytes());
        write_bytes_field(&mut body, 4, self.namespace.as_bytes());
        write_varint_field(&mut body, 5, self.payload_type);
        if let Some(payload) = &self.payload_utf8 {
            write_bytes_field(&mut body, 6, payload.as_bytes());
        }
        if let Some(payload) = &self.payload_binary {
            write_bytes_field(&mut body, 7, payload);
        }

        let mut frame = Vec::with_capacity(body.len() + 4);
        frame.extend_from_slice(&(body.len() as u32).to_be_bytes());
        frame.extend_from_slice(&body);
        frame
    }

    /// Parse the JSON payload of a STRING message with a size bound.
    pub(crate) fn payload_json(&self) -> Result<Value, CodecError> {
        let payload = self
            .payload_utf8
            .as_deref()
            .ok_or(CodecError::PayloadMissing)?;
        if payload.len() > MAX_JSON_BYTES {
            return Err(CodecError::JsonTooLarge(payload.len()));
        }
        serde_json::from_str(payload).map_err(|_| CodecError::InvalidJson)
    }
}

/// Decode a Cast frame body (without the 4-byte length prefix).
pub(crate) fn decode_cast_message(body: &[u8]) -> Result<CastMessage, CodecError> {
    let mut reader = Reader { buf: body, pos: 0 };
    let mut protocol_version = None;
    let mut source = None;
    let mut destination = None;
    let mut namespace = None;
    let mut payload_type = None;
    let mut payload_utf8 = None;
    let mut payload_binary = None;

    while reader.remaining() > 0 {
        let key = reader.varint()?;
        let field = key >> 3;
        let wire = (key & 0x07) as u8;
        if field == 0 {
            return Err(CodecError::FieldNumberZero);
        }
        if field > MAX_FIELD_NUMBER {
            return Err(CodecError::FieldNumberTooLarge);
        }
        match field {
            1 => {
                expect_wire("protocol_version", wire, WIRE_VARINT)?;
                protocol_version = Some(reader.varint()?);
            }
            2 => {
                expect_wire("source_id", wire, WIRE_LEN)?;
                source = Some(reader.string_field("source_id")?);
            }
            3 => {
                expect_wire("destination_id", wire, WIRE_LEN)?;
                destination = Some(reader.string_field("destination_id")?);
            }
            4 => {
                expect_wire("namespace", wire, WIRE_LEN)?;
                namespace = Some(reader.string_field("namespace")?);
            }
            5 => {
                expect_wire("payload_type", wire, WIRE_VARINT)?;
                payload_type = Some(reader.varint()?);
            }
            6 => {
                expect_wire("payload_utf8", wire, WIRE_LEN)?;
                payload_utf8 = Some(reader.string_field("payload_utf8")?);
            }
            7 => {
                expect_wire("payload_binary", wire, WIRE_LEN)?;
                payload_binary = Some(reader.len_delimited()?.to_vec());
            }
            _ => reader.skip(wire)?,
        }
    }

    let protocol_version = protocol_version.ok_or(CodecError::MissingField("protocol_version"))?;
    if protocol_version != 0 {
        return Err(CodecError::BadProtocolVersion(protocol_version));
    }
    let payload_type = payload_type.ok_or(CodecError::MissingField("payload_type"))?;
    if payload_type > 1 {
        return Err(CodecError::BadPayloadType(payload_type));
    }
    if payload_utf8.is_some() && payload_binary.is_some() {
        return Err(CodecError::DuplicatePayload);
    }
    if payload_utf8.is_none() && payload_binary.is_none() {
        return Err(CodecError::PayloadMissing);
    }
    if payload_type == 0 && payload_binary.is_some() {
        return Err(CodecError::PayloadTypeMismatch);
    }
    if payload_type == 1 && payload_utf8.is_some() {
        return Err(CodecError::PayloadTypeMismatch);
    }

    Ok(CastMessage {
        protocol_version,
        source: source.ok_or(CodecError::MissingField("source_id"))?,
        destination: destination.ok_or(CodecError::MissingField("destination_id"))?,
        namespace: namespace.ok_or(CodecError::MissingField("namespace"))?,
        payload_type,
        payload_utf8,
        payload_binary,
    })
}

fn expect_wire(field: &'static str, actual: u8, expected: u8) -> Result<(), CodecError> {
    if actual == expected {
        Ok(())
    } else {
        Err(CodecError::BadFieldType {
            field,
            wire: actual,
        })
    }
}

fn write_varint(out: &mut Vec<u8>, mut value: u64) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

fn write_tag(out: &mut Vec<u8>, field: u64, wire: u8) {
    write_varint(out, (field << 3) | u64::from(wire));
}

fn write_varint_field(out: &mut Vec<u8>, field: u64, value: u64) {
    write_tag(out, field, WIRE_VARINT);
    write_varint(out, value);
}

fn write_bytes_field(out: &mut Vec<u8>, field: u64, bytes: &[u8]) {
    write_tag(out, field, WIRE_LEN);
    write_varint(out, bytes.len() as u64);
    out.extend_from_slice(bytes);
}

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8], CodecError> {
        if self.remaining() < len {
            return Err(CodecError::UnexpectedEof("field"));
        }
        let out = &self.buf[self.pos..self.pos + len];
        self.pos += len;
        Ok(out)
    }

    fn varint(&mut self) -> Result<u64, CodecError> {
        let mut result = 0u64;
        let mut shift = 0u32;
        for _ in 0..10 {
            let byte = *self
                .buf
                .get(self.pos)
                .ok_or(CodecError::UnexpectedEof("varint"))?;
            self.pos += 1;
            let payload = u64::from(byte & 0x7f);
            if shift == 63 && payload > 1 {
                return Err(CodecError::VarintOverflow);
            }
            result |= payload << shift;
            if byte & 0x80 == 0 {
                return Ok(result);
            }
            shift += 7;
        }
        Err(CodecError::VarintTooLong)
    }

    fn len_delimited(&mut self) -> Result<&'a [u8], CodecError> {
        let len = self.varint()?;
        let len = usize::try_from(len).map_err(|_| CodecError::BadLength)?;
        self.take(len)
    }

    fn string_field(&mut self, field: &'static str) -> Result<String, CodecError> {
        let bytes = self.len_delimited()?;
        std::str::from_utf8(bytes)
            .map(str::to_owned)
            .map_err(|_| CodecError::InvalidUtf8(field))
    }

    fn skip(&mut self, wire: u8) -> Result<(), CodecError> {
        match wire {
            WIRE_VARINT => {
                self.varint()?;
                Ok(())
            }
            WIRE_FIXED64 => {
                self.take(8)?;
                Ok(())
            }
            WIRE_LEN => {
                let len = self.varint()?;
                let len = usize::try_from(len).map_err(|_| CodecError::BadLength)?;
                self.take(len)?;
                Ok(())
            }
            WIRE_FIXED32 => {
                self.take(4)?;
                Ok(())
            }
            WIRE_GROUP_START | WIRE_GROUP_END => Err(CodecError::GroupUnsupported),
            other => Err(CodecError::BadWireType(other)),
        }
    }
}

/// Incremental frame reader that retains partial-frame state across awaits.
///
/// Bytes are copied into owned buffers as they arrive, so dropping a read future
/// (for example when the caller cancels `stream`) leaves the in-progress header
/// and body in place for the next read.
#[derive(Debug, Default)]
pub(crate) struct FrameReader {
    header: [u8; 4],
    header_filled: usize,
    body: Vec<u8>,
    body_filled: usize,
    body_len: usize,
}

impl FrameReader {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Read the next frame body, or `None` on a clean end of stream.
    pub(crate) async fn next_frame<S>(&mut self, io: &mut S) -> Result<Option<Vec<u8>>, FrameError>
    where
        S: tokio::io::AsyncRead + Unpin,
    {
        use tokio::io::AsyncReadExt;

        loop {
            if self.header_filled < self.header.len() {
                let read = io.read(&mut self.header[self.header_filled..]).await?;
                if read == 0 {
                    if self.header_filled == 0 {
                        return Ok(None);
                    }
                    return Err(FrameError::Truncated);
                }
                self.header_filled += read;
                if self.header_filled == self.header.len() {
                    let len = u32::from_be_bytes(self.header) as usize;
                    if len == 0 {
                        return Err(FrameError::EmptyFrame);
                    }
                    if len > MAX_FRAME_BYTES {
                        return Err(FrameError::TooLarge(len));
                    }
                    self.body = vec![0u8; len];
                    self.body_filled = 0;
                    self.body_len = len;
                }
                continue;
            }

            if self.body_filled < self.body_len {
                let read = io.read(&mut self.body[self.body_filled..]).await?;
                if read == 0 {
                    return Err(FrameError::Truncated);
                }
                self.body_filled += read;
                continue;
            }

            let frame = std::mem::take(&mut self.body);
            self.header = [0u8; 4];
            self.header_filled = 0;
            self.body_filled = 0;
            self.body_len = 0;
            return Ok(Some(frame));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde_json::json;
    use tokio::io::AsyncWriteExt;

    use super::*;

    fn sample() -> CastMessage {
        CastMessage::json(
            "sender-0",
            "receiver-0",
            NS_HEARTBEAT,
            &json!({"type": "PING"}),
        )
    }

    fn body_of(message: &CastMessage) -> Vec<u8> {
        let frame = message.encode_frame();
        let len = u32::from_be_bytes(frame[..4].try_into().unwrap()) as usize;
        assert_eq!(len, frame.len() - 4);
        frame[4..].to_vec()
    }

    /// A syntactically valid minimal frame body with a JSON payload.
    fn minimal_body() -> Vec<u8> {
        let mut body = Vec::new();
        write_varint_field(&mut body, 1, 0);
        write_bytes_field(&mut body, 2, b"sender-0");
        write_bytes_field(&mut body, 3, b"receiver-0");
        write_bytes_field(&mut body, 4, NS_HEARTBEAT.as_bytes());
        write_varint_field(&mut body, 5, 0);
        write_bytes_field(&mut body, 6, b"{\"type\":\"PING\"}");
        body
    }

    #[test]
    fn round_trip_json_message() {
        let message = sample();
        let decoded = decode_cast_message(&body_of(&message)).unwrap();
        assert_eq!(decoded, message);
        assert_eq!(decoded.payload_json().unwrap(), json!({"type": "PING"}));
    }

    #[test]
    fn decodes_binary_payload() {
        let message = CastMessage {
            protocol_version: 0,
            source: "a".into(),
            destination: "b".into(),
            namespace: NS_MEDIA.into(),
            payload_type: 1,
            payload_utf8: None,
            payload_binary: Some(vec![1, 2, 3]),
        };
        let decoded = decode_cast_message(&body_of(&message)).unwrap();
        assert_eq!(decoded.payload_binary.as_deref(), Some(&[1, 2, 3][..]));
        assert_eq!(decoded.payload_json(), Err(CodecError::PayloadMissing));
    }

    #[test]
    fn skips_unknown_fields_safely() {
        let mut body = minimal_body();
        write_varint_field(&mut body, 10, 99);
        write_bytes_field(&mut body, 11, b"unknown");
        write_tag(&mut body, 12, WIRE_FIXED32);
        body.extend_from_slice(&[0u8; 4]);
        write_tag(&mut body, 13, WIRE_FIXED64);
        body.extend_from_slice(&[0u8; 8]);
        let decoded = decode_cast_message(&body).unwrap();
        assert_eq!(decoded.namespace, NS_HEARTBEAT);
    }

    #[test]
    fn rejects_missing_required_fields() {
        let mut no_version = Vec::new();
        write_bytes_field(&mut no_version, 2, b"sender-0");
        write_bytes_field(&mut no_version, 3, b"receiver-0");
        write_bytes_field(&mut no_version, 4, NS_HEARTBEAT.as_bytes());
        write_varint_field(&mut no_version, 5, 0);
        write_bytes_field(&mut no_version, 6, b"{}");
        assert_eq!(
            decode_cast_message(&no_version),
            Err(CodecError::MissingField("protocol_version"))
        );

        let mut no_source = Vec::new();
        write_varint_field(&mut no_source, 1, 0);
        write_bytes_field(&mut no_source, 3, b"receiver-0");
        write_bytes_field(&mut no_source, 4, NS_HEARTBEAT.as_bytes());
        write_varint_field(&mut no_source, 5, 0);
        write_bytes_field(&mut no_source, 6, b"{}");
        assert_eq!(
            decode_cast_message(&no_source),
            Err(CodecError::MissingField("source_id"))
        );
    }

    #[test]
    fn rejects_bad_protocol_version() {
        let mut body = Vec::new();
        write_varint_field(&mut body, 1, 2);
        write_bytes_field(&mut body, 2, b"sender-0");
        write_bytes_field(&mut body, 3, b"receiver-0");
        write_bytes_field(&mut body, 4, NS_HEARTBEAT.as_bytes());
        write_varint_field(&mut body, 5, 0);
        write_bytes_field(&mut body, 6, b"{}");
        assert_eq!(
            decode_cast_message(&body),
            Err(CodecError::BadProtocolVersion(2))
        );
    }

    #[test]
    fn rejects_bad_payload_type() {
        let mut body = Vec::new();
        write_varint_field(&mut body, 1, 0);
        write_bytes_field(&mut body, 2, b"sender-0");
        write_bytes_field(&mut body, 3, b"receiver-0");
        write_bytes_field(&mut body, 4, NS_HEARTBEAT.as_bytes());
        write_varint_field(&mut body, 5, 7);
        write_bytes_field(&mut body, 6, b"{}");
        assert_eq!(
            decode_cast_message(&body),
            Err(CodecError::BadPayloadType(7))
        );
    }

    #[test]
    fn rejects_wrong_wire_type_and_groups() {
        let mut wrong = Vec::new();
        write_tag(&mut wrong, 1, WIRE_LEN);
        write_bytes_field(&mut wrong, 1, b"0");
        assert!(matches!(
            decode_cast_message(&wrong),
            Err(CodecError::BadFieldType { .. })
        ));

        let mut group = minimal_body();
        write_tag(&mut group, 20, WIRE_GROUP_START);
        assert_eq!(
            decode_cast_message(&group),
            Err(CodecError::GroupUnsupported)
        );

        let mut bad_wire = minimal_body();
        write_tag(&mut bad_wire, 21, 7);
        assert_eq!(
            decode_cast_message(&bad_wire),
            Err(CodecError::BadWireType(7))
        );
    }

    #[test]
    fn rejects_field_zero_and_huge_field_numbers() {
        let mut field_zero = minimal_body();
        write_varint(&mut field_zero, 0);
        assert_eq!(
            decode_cast_message(&field_zero),
            Err(CodecError::FieldNumberZero)
        );

        let mut huge = minimal_body();
        write_tag(&mut huge, MAX_FIELD_NUMBER + 1, WIRE_VARINT);
        write_varint(&mut huge, 1);
        assert_eq!(
            decode_cast_message(&huge),
            Err(CodecError::FieldNumberTooLarge)
        );
    }

    #[test]
    fn rejects_invalid_utf8_and_truncated_lengths() {
        let mut invalid_utf8 = Vec::new();
        write_varint_field(&mut invalid_utf8, 1, 0);
        write_bytes_field(&mut invalid_utf8, 2, b"\xff\xfe");
        assert!(matches!(
            decode_cast_message(&invalid_utf8),
            Err(CodecError::InvalidUtf8("source_id"))
        ));

        let mut truncated = Vec::new();
        write_tag(&mut truncated, 2, WIRE_LEN);
        write_varint(&mut truncated, 64);
        truncated.extend_from_slice(b"short");
        assert_eq!(
            decode_cast_message(&truncated),
            Err(CodecError::UnexpectedEof("field"))
        );
    }

    #[test]
    fn rejects_malformed_varints() {
        let mut overlong = Vec::new();
        overlong.extend_from_slice(&[0x80; 11]);
        assert_eq!(
            decode_cast_message(&overlong),
            Err(CodecError::VarintTooLong)
        );

        let mut overflow = Vec::new();
        // Ten-byte varint whose last byte has more than one significant bit.
        overflow.extend_from_slice(&[0xff; 9]);
        overflow.push(0x7f);
        assert_eq!(
            decode_cast_message(&overflow),
            Err(CodecError::VarintOverflow)
        );

        // A truncated varint at the very end of the buffer.
        let mut cut = minimal_body();
        cut.push(0x80);
        assert_eq!(
            decode_cast_message(&cut),
            Err(CodecError::UnexpectedEof("varint"))
        );
    }

    #[test]
    fn rejects_conflicting_or_missing_payloads() {
        let mut both = Vec::new();
        write_varint_field(&mut both, 1, 0);
        write_bytes_field(&mut both, 2, b"sender-0");
        write_bytes_field(&mut both, 3, b"receiver-0");
        write_bytes_field(&mut both, 4, NS_HEARTBEAT.as_bytes());
        write_varint_field(&mut both, 5, 0);
        write_bytes_field(&mut both, 6, b"{}");
        write_bytes_field(&mut both, 7, b"\x01");
        assert_eq!(
            decode_cast_message(&both),
            Err(CodecError::DuplicatePayload)
        );

        let mut none = Vec::new();
        write_varint_field(&mut none, 1, 0);
        write_bytes_field(&mut none, 2, b"sender-0");
        write_bytes_field(&mut none, 3, b"receiver-0");
        write_bytes_field(&mut none, 4, NS_HEARTBEAT.as_bytes());
        write_varint_field(&mut none, 5, 0);
        assert_eq!(decode_cast_message(&none), Err(CodecError::PayloadMissing));

        let mut mismatch = Vec::new();
        write_varint_field(&mut mismatch, 1, 0);
        write_bytes_field(&mut mismatch, 2, b"sender-0");
        write_bytes_field(&mut mismatch, 3, b"receiver-0");
        write_bytes_field(&mut mismatch, 4, NS_HEARTBEAT.as_bytes());
        write_varint_field(&mut mismatch, 5, 1);
        write_bytes_field(&mut mismatch, 6, b"{}");
        assert_eq!(
            decode_cast_message(&mismatch),
            Err(CodecError::PayloadTypeMismatch)
        );
    }

    #[test]
    fn rejects_oversized_json() {
        let payload = "x".repeat(MAX_JSON_BYTES + 1);
        let message = CastMessage {
            protocol_version: 0,
            source: "a".into(),
            destination: "b".into(),
            namespace: NS_RECEIVER.into(),
            payload_type: 0,
            payload_utf8: Some(payload.clone()),
            payload_binary: None,
        };
        assert_eq!(
            message.payload_json(),
            Err(CodecError::JsonTooLarge(payload.len()))
        );
    }

    #[test]
    fn rejects_invalid_json() {
        let message = CastMessage {
            protocol_version: 0,
            source: "a".into(),
            destination: "b".into(),
            namespace: NS_RECEIVER.into(),
            payload_type: 0,
            payload_utf8: Some("{not json".into()),
            payload_binary: None,
        };
        assert_eq!(message.payload_json(), Err(CodecError::InvalidJson));
    }

    #[tokio::test]
    async fn frame_reader_rejects_bad_lengths() {
        let (mut client, mut server) = tokio::io::duplex(128);

        server.write_all(&0u32.to_be_bytes()).await.unwrap();
        let mut reader = FrameReader::new();
        assert!(matches!(
            reader.next_frame(&mut client).await,
            Err(FrameError::EmptyFrame)
        ));

        server
            .write_all(&((MAX_FRAME_BYTES as u32) + 1).to_be_bytes())
            .await
            .unwrap();
        let mut reader = FrameReader::new();
        assert!(matches!(
            reader.next_frame(&mut client).await,
            Err(FrameError::TooLarge(_))
        ));
    }

    #[tokio::test]
    async fn frame_reader_reports_truncated_frames() {
        let (mut client, mut server) = tokio::io::duplex(128);
        let frame = sample().encode_frame();
        server.write_all(&frame[..frame.len() - 1]).await.unwrap();
        drop(server);

        let mut reader = FrameReader::new();
        assert!(matches!(
            reader.next_frame(&mut client).await,
            Err(FrameError::Truncated)
        ));
    }

    #[tokio::test]
    async fn frame_reader_retains_partial_state_across_cancellation() {
        let (mut client, mut server) = tokio::io::duplex(1024);
        let message = sample();
        let frame = message.encode_frame();
        let body = body_of(&message);

        server.write_all(&frame[..3]).await.unwrap();
        let mut reader = FrameReader::new();
        let partial =
            tokio::time::timeout(Duration::from_millis(20), reader.next_frame(&mut client)).await;
        assert!(partial.is_err(), "partial frame must not be yielded");

        server.write_all(&frame[3..]).await.unwrap();
        let complete =
            tokio::time::timeout(Duration::from_millis(200), reader.next_frame(&mut client))
                .await
                .expect("frame did not complete")
                .expect("frame read failed")
                .expect("stream ended");
        assert_eq!(complete, body);
        assert_eq!(decode_cast_message(&complete).unwrap(), message);
    }
}
