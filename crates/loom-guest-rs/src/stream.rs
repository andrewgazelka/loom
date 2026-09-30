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
}

impl std::fmt::Display for StreamError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Cancelled => "the consumer of this stream is gone",
            Self::NotStreaming => "this execution was not started as a stream",
            Self::Denied => "the yield effect is not allowed here",
            Self::Encode => "the value could not be encoded",
        })
    }
}
impl std::error::Error for StreamError {}

/// Send `value` (DAG-CBOR, like every result) to the consumer.
pub fn emit<T: Serialize>(value: &T) -> Result<(), StreamError> {
    let bytes = loom_proto::isolated::encode_payload(value).map_err(|_| StreamError::Encode)?;
    crate::core::yield_value(&bytes)
}
