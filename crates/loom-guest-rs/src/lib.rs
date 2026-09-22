//! Synchronous guest interface to the core wasm Loom host.
#[doc(hidden)]
pub mod core;
mod detached;
mod scoped;
pub use detached::{JoinHandle, spawn};
pub use scoped::{Scope, ScopedJoinHandle, scope};
mod handlers;
pub mod preview;
pub use handlers::{Continuation, Effect, Reply, handle, handle_any, handle_pinned};
pub mod isolated;
pub use isolated::CallError;
pub use serde;
use serde::{Serialize, de::DeserializeOwned};
pub use serde_json;

pub type EffectError = String;
pub use loom_proto::{Bytes, DirEntry, EntryKind, TypeSig, Value, decode, decode_host, encode};

/// Perform an effect and decode its result. The call suspends until it completes.
pub fn perform<T: DeserializeOwned>(name: &str, args: impl Serialize) -> Result<T, EffectError> {
    let args = serde_json::to_value(args).map_err(|error| error.to_string())?;
    let desc = loom_proto::Desc::<T>::new(name, args);
    let bytes = encode(&desc)?;
    core::perform(&bytes)
}

pub fn now() -> Result<Value, EffectError> {
    perform("now", Value::Null)
}
pub fn random() -> Result<f64, EffectError> {
    perform("random", Value::Null)
}
pub fn sleep(ms: u64) -> Result<(), EffectError> {
    perform("sleep", serde_json::json!({"ms": ms}))
}
pub fn exec(args: Value) -> Result<Value, EffectError> {
    perform("exec", args)
}
pub fn llm(args: Value) -> Result<Value, EffectError> {
    perform("llm", args)
}
pub mod actor;

pub mod fs {
    use super::*;
    pub fn list(machine: &str, path: &str) -> Result<Vec<DirEntry>, EffectError> {
        perform(
            "fs.list",
            serde_json::json!({"machine":machine,"path":path}),
        )
    }
    pub fn stat(machine: &str, path: &str) -> Result<DirEntry, EffectError> {
        perform(
            "fs.stat",
            serde_json::json!({"machine":machine,"path":path}),
        )
    }
    pub fn walk(
        machine: &str,
        path: &str,
        max_depth: u32,
        max_entries: u32,
    ) -> Result<Vec<DirEntry>, EffectError> {
        perform(
            "fs.walk",
            serde_json::json!({"machine":machine,"path":path,"max_depth":max_depth,"max_entries":max_entries}),
        )
    }
    pub fn read(machine: &str, path: &str) -> Result<String, EffectError> {
        perform(
            "fs.read",
            serde_json::json!({"machine":machine,"path":path}),
        )
    }
    /// Read a UTF-8 file, returning None only when the final path is absent.
    pub fn read_optional(machine: &str, path: &str) -> Result<Option<String>, EffectError> {
        perform(
            "fs.read_optional",
            serde_json::json!({"machine":machine,"path":path}),
        )
    }
    /// Replace a UTF-8 file relative to a machine's pinned filesystem root.
    pub fn write(machine: &str, path: &str, content: &str) -> Result<(), EffectError> {
        perform(
            "fs.write",
            serde_json::json!({"machine":machine,"path":path,"content":content}),
        )
    }
    pub fn snapshot(machine: Value, path: &str) -> Result<Value, EffectError> {
        perform(
            "fs.snapshot",
            serde_json::json!({"machine":machine,"path":path}),
        )
    }
}

pub mod cas {
    use super::*;

    pub fn get<T: DeserializeOwned>(hash: &str) -> Result<T, EffectError> {
        perform("cas.get", serde_json::json!({"hash": hash}))
    }

    pub fn put(value: impl Serialize) -> Result<Value, EffectError> {
        perform("cas.put", value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn wire_values_roundtrip_and_reject_trailing_data() {
        let reference =
            loom_proto::reference(&"ab".repeat(32), loom_proto::DAG_CBOR_CODEC).unwrap();
        let value = serde_json::json!({"ref":reference,"nested":[null,true,-3,1.5]});
        let mut bytes = encode(&value).unwrap();
        assert_eq!(decode::<Value>(&bytes).unwrap(), value);
        bytes.push(0);
        assert!(decode::<Value>(&bytes).is_err());
    }
}
