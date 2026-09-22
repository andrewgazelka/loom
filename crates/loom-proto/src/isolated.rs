//! The isolated-call wire. An isolated call runs another definition in a
//! fresh wasm instance. It is the isolation boundary (actor boundaries,
//! untrusted code, cross-language), not how code calls code: code calls code
//! by static linking on the definition hash (`loom-build` compiles definition
//! dependencies as rlibs, `crates/loom-build/src/materialize.rs`).
//!
//! The host reads ONLY the request header and the one-byte response tag. The
//! argument payload and the result are opaque DAG-CBOR that only the two
//! guests decode, so each direction costs exactly two codec passes:
//!
//! | direction | pass 1                             | pass 2                              |
//! |-----------|------------------------------------|-------------------------------------|
//! | arguments | caller `encode_payload` (typed)    | callee `decode_payload` (typed)     |
//! | result    | callee `encode_payload` (typed)    | caller `decode_payload` (typed)     |
//!
//! A JavaScript callee or caller adds one `Value` materialization on its own
//! side of the boundary, because V8 has no DAG-CBOR; Rust-to-Rust stays at two.
//!
//! Request frame (`Request::encode` / `Request::parse`):
//!
//! | offset  | size | field                                                     |
//! |---------|------|-----------------------------------------------------------|
//! | 0       | 1    | `VERSION` (1)                                             |
//! | 1       | 1    | target: 0 = the digest below, 1 = the calling definition  |
//! | 2       | 32   | definition BLAKE3-256 digest (all zero for target 1)      |
//! | 34      | 4    | entry name length `n`, little-endian u32 (`n <= 256`)     |
//! | 38      | n    | entry name, UTF-8; `n = 0` selects the single export      |
//! | 38 + n  | 4    | argument count, little-endian u32                         |
//! | 42 + n  | rest | payload: one DAG-CBOR array of the typed arguments        |
//!
//! Response frame (`response_frame` / `Response::parse`):
//!
//! | offset | size | field                                                          |
//! |--------|------|----------------------------------------------------------------|
//! | 0      | 1    | tag: 0 = ok, 1 = error                                         |
//! | 1      | rest | ok: the callee's DAG-CBOR result; error: `CallError` as a map  |
//!
//! Integers travel as DAG-CBOR major types 0 and 1 (full u64 and i64 range),
//! byte strings as major type 2 (`crate::Bytes`). Only results that leave the
//! Rust boundary (the host `Value` API, JavaScript) must fit the JSON `Value`
//! model; the boundary refuses them there, never silently.
use serde::{Deserialize, Serialize, de::DeserializeOwned};

/// Bumped whenever the frame layout changes; a mismatch is a `Decode` error.
pub const VERSION: u8 = 1;
/// Nested isolated calls beyond this depth fail before the callee is
/// instantiated. Each level is one fresh instance plus one suspended host
/// call, so the limit bounds host memory for a self-recursive definition.
pub const MAX_DEPTH: u32 = 64;
pub const DIGEST_BYTES: usize = 32;
/// Entry names are Rust identifiers; anything longer is a malformed header.
pub const MAX_ENTRY_BYTES: usize = 256;
const FIXED_HEADER_BYTES: usize = 1 + 1 + DIGEST_BYTES + 4;
const TARGET_DIGEST: u8 = 0;
const TARGET_THIS: u8 = 1;
const TAG_OK: u8 = 0;
const TAG_ERROR: u8 = 1;
/// The literal a guest writes to name the definition it is running in.
pub const SELF_LABEL: &str = "$self";

/// Which definition an isolated call runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Target {
    /// The calling definition itself (`"$self"`); the host substitutes the
    /// running definition's hash from its effect context.
    This,
    /// A definition by its BLAKE3-256 hash.
    Hash([u8; DIGEST_BYTES]),
}
const fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}
/// Parse 64 hex characters at compile time; `None` for any other input.
pub const fn parse_digest(text: &str) -> Option<[u8; DIGEST_BYTES]> {
    let bytes = text.as_bytes();
    if bytes.len() != DIGEST_BYTES * 2 {
        return None;
    }
    let mut digest = [0u8; DIGEST_BYTES];
    let mut index = 0;
    while index < DIGEST_BYTES {
        let high = match hex_nibble(bytes[index * 2]) {
            Some(value) => value,
            None => return None,
        };
        let low = match hex_nibble(bytes[index * 2 + 1]) {
            Some(value) => value,
            None => return None,
        };
        digest[index] = (high << 4) | low;
        index += 1;
    }
    Some(digest)
}
impl Target {
    /// `"$self"` or 64 hex characters, usable in `const` items.
    pub const fn parse(text: &str) -> Option<Self> {
        if text.len() == SELF_LABEL.len() {
            let bytes = text.as_bytes();
            let label = SELF_LABEL.as_bytes();
            let mut index = 0;
            let mut same = true;
            while index < label.len() {
                same &= bytes[index] == label[index];
                index += 1;
            }
            if same {
                return Some(Self::This);
            }
        }
        match parse_digest(text) {
            Some(digest) => Some(Self::Hash(digest)),
            None => None,
        }
    }
    /// The run-time form of `parse`, with the failure named.
    pub fn from_hex(text: &str) -> Result<Self, CallError> {
        Self::parse(text).ok_or_else(|| CallError::Decode {
            message: format!(
                "definition target must be \"$self\" or 64 hex characters, got {text:?}"
            ),
        })
    }
    /// `"$self"` or the lowercase hex digest: the store's definition hash form.
    pub fn label(&self) -> String {
        match self {
            Self::This => SELF_LABEL.to_owned(),
            Self::Hash(digest) => hex::encode(digest),
        }
    }
}

/// One isolated call as the host sees it: the header fields plus the opaque
/// payload. Borrowed so parsing a frame copies nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Request<'a> {
    pub target: Target,
    /// Empty selects the definition's single export.
    pub entry: &'a str,
    /// Declared by the caller from its typed arity; the host checks it against
    /// the stored signature before instantiating the callee.
    pub argc: u32,
    /// One DAG-CBOR array of `argc` typed arguments. The host never decodes it.
    pub payload: &'a [u8],
}
impl<'a> Request<'a> {
    pub fn encode(&self) -> Vec<u8> {
        let mut frame =
            Vec::with_capacity(FIXED_HEADER_BYTES + self.entry.len() + 4 + self.payload.len());
        frame.push(VERSION);
        match self.target {
            Target::This => {
                frame.push(TARGET_THIS);
                frame.extend_from_slice(&[0u8; DIGEST_BYTES]);
            }
            Target::Hash(digest) => {
                frame.push(TARGET_DIGEST);
                frame.extend_from_slice(&digest);
            }
        }
        frame.extend_from_slice(&(self.entry.len() as u32).to_le_bytes());
        frame.extend_from_slice(self.entry.as_bytes());
        frame.extend_from_slice(&self.argc.to_le_bytes());
        frame.extend_from_slice(self.payload);
        frame
    }
    /// Parse the header; the payload is the untouched remainder.
    pub fn parse(frame: &'a [u8]) -> Result<Self, CallError> {
        let malformed = |message: &str| CallError::Decode {
            message: format!("malformed isolated call header: {message}"),
        };
        if frame.len() < FIXED_HEADER_BYTES {
            return Err(malformed("shorter than the fixed header"));
        }
        if frame[0] != VERSION {
            return Err(malformed(&format!("version {} is not {VERSION}", frame[0])));
        }
        let digest: [u8; DIGEST_BYTES] = frame[2..2 + DIGEST_BYTES]
            .try_into()
            .expect("fixed header holds the digest");
        let target = match frame[1] {
            TARGET_DIGEST => Target::Hash(digest),
            TARGET_THIS if digest == [0u8; DIGEST_BYTES] => Target::This,
            TARGET_THIS => return Err(malformed("$self target carries a digest")),
            kind => return Err(malformed(&format!("unknown target kind {kind}"))),
        };
        let mut offset = 2 + DIGEST_BYTES;
        let entry_len = u32::from_le_bytes(
            frame[offset..offset + 4]
                .try_into()
                .expect("fixed header holds the entry length"),
        ) as usize;
        offset += 4;
        if entry_len > MAX_ENTRY_BYTES {
            return Err(malformed("entry name exceeds 256 bytes"));
        }
        let entry = frame
            .get(offset..offset + entry_len)
            .ok_or_else(|| malformed("entry name runs past the frame"))?;
        let entry = std::str::from_utf8(entry).map_err(|_| malformed("entry name is not UTF-8"))?;
        offset += entry_len;
        let argc = frame
            .get(offset..offset + 4)
            .ok_or_else(|| malformed("argument count runs past the frame"))?;
        let argc = u32::from_le_bytes(argc.try_into().expect("four bytes"));
        offset += 4;
        Ok(Self {
            target,
            entry,
            argc,
            payload: &frame[offset..],
        })
    }
}

/// Why an isolated call did not return the callee's result. Serialized as a
/// DAG-CBOR map with the variant name as its single key.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CallError {
    /// No executable definition has this hash.
    NotFound { hash: String },
    /// The caller's effect policy does not permit `effect` (always `"call"`
    /// today) towards `hash`.
    Denied { effect: String, hash: String },
    /// The callee started and failed: a trap, a denied nested effect, a host
    /// failure while it ran. `message` is the flattened cause.
    Trapped { hash: String, message: String },
    /// A codec failure at either end of the payload or a malformed frame.
    Decode { message: String },
    /// The header's argument count disagrees with the callee's stored
    /// signature; reported before the callee is instantiated.
    Arity {
        hash: String,
        entry: String,
        expected: u32,
        actual: u32,
    },
    /// Nesting reached `MAX_DEPTH`; reported before the callee is instantiated.
    DepthExceeded { depth: u32 },
}
impl std::fmt::Display for CallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound { hash } => write!(f, "definition {hash} not found"),
            Self::Denied { effect, hash } => {
                write!(
                    f,
                    "effect {effect} towards definition {hash} is not allowed"
                )
            }
            Self::Trapped { hash, message } => write!(f, "definition {hash} failed: {message}"),
            Self::Decode { message } => write!(f, "isolated call codec: {message}"),
            Self::Arity {
                hash,
                entry,
                expected,
                actual,
            } => write!(
                f,
                "definition {hash} entry {entry} takes {expected} arguments, the call declares {actual}"
            ),
            Self::DepthExceeded { depth } => {
                write!(
                    f,
                    "isolated call depth {depth} reached the limit {MAX_DEPTH}"
                )
            }
        }
    }
}
impl std::error::Error for CallError {}

/// Typed argument or result payload: one direct DAG-CBOR pass, no `Value`
/// tree. Argument tuples become the payload array; a result is one value.
pub fn encode_payload<T: Serialize + ?Sized>(value: &T) -> Result<Vec<u8>, CallError> {
    serde_ipld_dagcbor::to_vec(value).map_err(|error| CallError::Decode {
        message: format!("encode: {error}"),
    })
}
/// The inverse of `encode_payload`. Trailing bytes, including unconsumed
/// argument elements past a fixed-arity tuple, are rejected.
pub fn decode_payload<T: DeserializeOwned>(payload: &[u8]) -> Result<T, CallError> {
    let mut deserializer = serde_ipld_dagcbor::de::Deserializer::from_slice(payload);
    let value = T::deserialize(&mut deserializer).map_err(|error| CallError::Decode {
        message: format!("decode: {error}"),
    })?;
    deserializer.end().map_err(|error| CallError::Decode {
        message: format!("decode: {error}"),
    })?;
    Ok(value)
}

/// Build the response frame around an already encoded result or an error.
pub fn response_frame(result: Result<&[u8], &CallError>) -> Vec<u8> {
    match result {
        Ok(payload) => {
            let mut frame = Vec::with_capacity(1 + payload.len());
            frame.push(TAG_OK);
            frame.extend_from_slice(payload);
            frame
        }
        Err(error) => {
            let mut frame = vec![TAG_ERROR];
            frame.extend(
                serde_ipld_dagcbor::to_vec(error)
                    .expect("CallError holds only strings and integers"),
            );
            frame
        }
    }
}
pub struct Response;
impl Response {
    /// Split a response frame. The outer `Err` is a malformed frame (a guest
    /// protocol violation); the inner `Err` is the callee's structured failure.
    pub fn parse(frame: &[u8]) -> Result<Result<&[u8], CallError>, String> {
        match frame.split_first() {
            Some((&TAG_OK, payload)) => Ok(Ok(payload)),
            Some((&TAG_ERROR, error)) => Ok(Err(decode_payload::<CallError>(error)
                .map_err(|error| format!("error frame does not hold a CallError: {error}"))?)),
            Some((tag, _)) => Err(format!("unknown response tag {tag}")),
            None => Err("empty response frame".into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Bytes;

    const DIGEST_HEX: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";

    #[test]
    fn target_parses_self_and_hex_at_compile_time() {
        const THIS: Option<Target> = Target::parse("$self");
        const HASH: Option<Target> = Target::parse(DIGEST_HEX);
        assert_eq!(THIS, Some(Target::This));
        let Some(Target::Hash(digest)) = HASH else {
            panic!("hex digest");
        };
        assert_eq!(digest[0], 0x00);
        assert_eq!(digest[1], 0x11);
        assert_eq!(digest[31], 0xff);
        assert_eq!(Target::parse("$SELF"), None);
        assert_eq!(Target::parse(&"0".repeat(63)), None);
        assert_eq!(Target::parse(&"g".repeat(64)), None);
        assert_eq!(Target::Hash(digest).label(), DIGEST_HEX);
        assert_eq!(Target::This.label(), "$self");
        assert!(matches!(
            Target::from_hex("nope"),
            Err(CallError::Decode { .. })
        ));
    }

    #[test]
    fn request_frame_round_trips_and_the_payload_is_untouched() {
        let payload = encode_payload(&(7u8, "x")).unwrap();
        let request = Request {
            target: Target::parse(DIGEST_HEX).unwrap(),
            entry: "main",
            argc: 2,
            payload: &payload,
        };
        let frame = request.encode();
        assert_eq!(frame.len(), 42 + 4 + payload.len());
        assert_eq!(frame[0], VERSION);
        assert_eq!(frame[1], TARGET_DIGEST);
        assert_eq!(&frame[34..38], &4u32.to_le_bytes());
        assert_eq!(&frame[38..42], b"main");
        assert_eq!(&frame[42..46], &2u32.to_le_bytes());
        let parsed = Request::parse(&frame).unwrap();
        assert_eq!(parsed, request);
        assert!(std::ptr::eq(parsed.payload.as_ptr(), &frame[46]));

        let this = Request {
            target: Target::This,
            entry: "",
            argc: 0,
            payload: &[0x80],
        };
        let frame = this.encode();
        assert_eq!(frame[1], TARGET_THIS);
        assert_eq!(Request::parse(&frame).unwrap(), this);
    }

    #[test]
    fn malformed_request_headers_are_decode_errors() {
        let good = Request {
            target: Target::This,
            entry: "main",
            argc: 0,
            payload: &[0x80],
        }
        .encode();
        let mut wrong_version = good.clone();
        wrong_version[0] = 2;
        let mut self_with_digest = good.clone();
        self_with_digest[2] = 1;
        let mut unknown_target = good.clone();
        unknown_target[1] = 9;
        let mut long_entry = good.clone();
        long_entry[34..38].copy_from_slice(&(MAX_ENTRY_BYTES as u32 + 1).to_le_bytes());
        let mut runaway_entry = good.clone();
        runaway_entry[34..38].copy_from_slice(&200u32.to_le_bytes());
        let mut bad_utf8 = good.clone();
        bad_utf8[38] = 0xff;
        for frame in [
            &good[..FIXED_HEADER_BYTES - 1],
            &wrong_version[..],
            &self_with_digest[..],
            &unknown_target[..],
            &long_entry[..],
            &runaway_entry[..],
            &bad_utf8[..],
            &good[..good.len() - 2],
        ] {
            assert!(
                matches!(Request::parse(frame), Err(CallError::Decode { .. })),
                "{frame:?}"
            );
        }
        // Frame truncated exactly after the argument count: an empty payload
        // parses; the callee reports the missing array, not the header parser.
        let headless = &good[..good.len() - 1];
        assert_eq!(Request::parse(headless).unwrap().payload, &[] as &[u8]);
    }

    #[test]
    fn integers_and_bytes_keep_full_fidelity() {
        let payload = encode_payload(&(u64::MAX, i64::MIN, -1i8)).unwrap();
        // major 0 with 8-byte argument, major 1 with 8-byte argument, one byte.
        assert_eq!(
            payload,
            [
                0x83, 0x1b, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x3b, 0x7f, 0xff, 0xff,
                0xff, 0xff, 0xff, 0xff, 0xff, 0x20
            ]
        );
        let (max, min, minus): (u64, i64, i8) = decode_payload(&payload).unwrap();
        assert_eq!((max, min, minus), (u64::MAX, i64::MIN, -1));
        // The JSON Value boundary refuses what it cannot represent.
        assert!(crate::decode_host::<crate::Value>(&payload).is_err());

        let blob = Bytes::from(vec![0x7e; 1 << 20]);
        let payload = encode_payload(&(blob.clone(),)).unwrap();
        // array(1) + bytes major type with a 4-byte length: 1 + 5 header bytes.
        assert_eq!(payload.len(), (1 << 20) + 6);
        assert_eq!(&payload[..6], [0x81, 0x5a, 0x00, 0x10, 0x00, 0x00]);
        let (echoed,): (Bytes,) = decode_payload(&payload).unwrap();
        assert_eq!(echoed, blob);
        assert!(crate::decode_host::<crate::Value>(&payload).is_err());
    }

    #[test]
    fn decode_rejects_trailing_bytes_and_surplus_arguments() {
        let mut payload = encode_payload(&(1u8,)).unwrap();
        payload.push(0);
        assert!(matches!(
            decode_payload::<(u8,)>(&payload),
            Err(CallError::Decode { .. })
        ));
        let surplus = encode_payload(&(1u8, 2u8)).unwrap();
        assert!(matches!(
            decode_payload::<(u8,)>(&surplus),
            Err(CallError::Decode { .. })
        ));
        let empty: [(); 0] = decode_payload(&[0x80]).unwrap();
        assert!(empty.is_empty());
    }

    #[test]
    fn response_frames_carry_results_or_structured_errors() {
        let result = encode_payload(&42u32).unwrap();
        let frame = response_frame(Ok(&result));
        assert_eq!(frame[0], TAG_OK);
        assert_eq!(Response::parse(&frame).unwrap().unwrap(), &result[..]);

        let error = CallError::DepthExceeded { depth: 64 };
        let frame = response_frame(Err(&error));
        assert_eq!(frame[0], TAG_ERROR);
        assert_eq!(Response::parse(&frame).unwrap().unwrap_err(), error);
        let decoded: crate::Value = crate::decode_host(&frame[1..]).unwrap();
        assert_eq!(
            decoded,
            serde_json::json!({"depth_exceeded": {"depth": 64}})
        );

        for variant in [
            CallError::NotFound { hash: "h".into() },
            CallError::Denied {
                effect: "call".into(),
                hash: "h".into(),
            },
            CallError::Trapped {
                hash: "h".into(),
                message: "boom".into(),
            },
            CallError::Decode {
                message: "bad".into(),
            },
            CallError::Arity {
                hash: "h".into(),
                entry: "main".into(),
                expected: 1,
                actual: 2,
            },
        ] {
            let frame = response_frame(Err(&variant));
            assert_eq!(Response::parse(&frame).unwrap().unwrap_err(), variant);
        }

        assert!(Response::parse(&[]).is_err());
        assert!(Response::parse(&[7]).is_err());
        assert!(Response::parse(&[TAG_ERROR, 0x80]).is_err());
    }
}
