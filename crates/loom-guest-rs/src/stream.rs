//! Generators: an entry that yields many values before it returns.
//!
//! Call [`emit`] anywhere in an entry; each value reaches the consumer of the call
//! (`Runtime::call_stream` on the host). `emit` returns once the consumer has room, so a
//! slow consumer slows the producer. When it returns [`StreamError::Cancelled`] the consumer has
//! gone: stop and return. The entry's own return value is delivered last, as the stream's result.
//!
//! ```ignore
//! pub fn panels(count: u32) -> Result<u32, String> {
//!     for index in 0..count {
//!         loom::stream::emit(&build_panel(index)).map_err(|e| e.to_string())?;
//!     }
//!     Ok(count)
//! }
//! ```
//! An entry that yields carries the fixed effect label `yield`, so it is never answered from the
//! result cache and its callers must allow `yield`.
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamError {
    /// The consumer stopped listening; end the entry.
    Cancelled,
    /// This execution was not started as a stream.
    NotStreaming,
    /// The `yield` effect is not allowed here.
    Denied,
    /// The value could not be encoded.
    Encode,
    /// The encoded value is larger than the host accepts for one item (16 MiB); it was not sent.
    /// Split it into several items, or store it and send a handle.
    TooLarge,
    /// The host answered with a yield code this SDK does not know: an SDK older than its host.
    Unknown(i32),
}

impl std::fmt::Display for StreamError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cancelled => f.write_str("the consumer of this stream is gone"),
            Self::NotStreaming => f.write_str("this execution was not started as a stream"),
            Self::Denied => f.write_str("the yield effect is not allowed here"),
            Self::Encode => f.write_str("the value could not be encoded"),
            Self::TooLarge => f.write_str("the value is larger than one stream item may be"),
            Self::Unknown(code) => {
                write!(f, "the host answered the yield with an unknown code {code}")
            }
        }
    }
}
impl std::error::Error for StreamError {}

/// Send `value` (DAG-CBOR, like every result) to the consumer.
pub fn emit<T: Serialize>(value: &T) -> Result<(), StreamError> {
    let bytes = loom_proto::isolated::encode_payload(value).map_err(|_| StreamError::Encode)?;
    crate::core::yield_value(&bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unknown_yield_code_is_named_in_the_message() {
        assert!(StreamError::Unknown(9).to_string().contains('9'));
        assert_ne!(StreamError::Unknown(9), StreamError::Denied);
    }
}
