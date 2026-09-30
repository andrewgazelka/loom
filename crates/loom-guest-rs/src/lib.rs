//! Synchronous guest interface to the core wasm Loom host.
#![cfg_attr(target_arch = "wasm32", feature(allow_internal_unsafe))]
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
pub mod kernel;
pub use isolated::CallError;
pub use glam;
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

/// The exported `loom_call_<entry>` wrapper for one root `pub fn`, appended by
/// the host after the checked guest source (`loom-build` `entry_abi`), so the
/// guest compiles once. `$name` is the entry; the identifiers after `;` are
/// one binding per parameter. The payload is one DAG-CBOR array of typed
/// arguments decoded straight into a tuple whose element types rustc infers
/// from the entry's own parameter types (arity 0 is `[(); 0]`, since `()` would
/// be CBOR null). Codec passes: one decode of the arguments, one encode of the
/// result; the host copies both payloads without decoding.
///
/// DWARF lines for wrapper instructions name this file (the macro's own
/// lines), not the guest's: rustc does not collapse them to the call site.
///
/// The host appends the call to this macro after the guest's source; guest
/// source cannot invoke it, because the checker refuses guest macros.
#[doc(hidden)]
#[macro_export]
#[cfg_attr(target_arch = "wasm32", allow_internal_unsafe)]
macro_rules! __loom_export_entry {
    ($name:ident;) => {
        const _: () = {
            #[unsafe(export_name = concat!("loom_call_", stringify!($name)))]
            extern "C" fn entry(
                pointer: ::core::primitive::u32,
                length: ::core::primitive::u32,
            ) -> ::core::primitive::u64 {
                let invoke = || -> ::std::result::Result<
                    ::std::vec::Vec<::core::primitive::u8>,
                    $crate::CallError,
                > {
                    let bytes = unsafe { $crate::core::input(pointer, length) };
                    let []: [(); 0] = $crate::isolated::decode_payload(bytes)?;
                    $crate::isolated::encode_payload(&crate::$name())
                };
                $crate::core::isolated_response(invoke())
            }
        };
    };
    ($name:ident; $($argument:ident),+) => {
        const _: () = {
            #[unsafe(export_name = concat!("loom_call_", stringify!($name)))]
            extern "C" fn entry(
                pointer: ::core::primitive::u32,
                length: ::core::primitive::u32,
            ) -> ::core::primitive::u64 {
                let invoke = || -> ::std::result::Result<
                    ::std::vec::Vec<::core::primitive::u8>,
                    $crate::CallError,
                > {
                    let bytes = unsafe { $crate::core::input(pointer, length) };
                    let ($($argument,)+) = $crate::isolated::decode_payload(bytes)?;
                    $crate::isolated::encode_payload(&crate::$name($($argument),+))
                };
                $crate::core::isolated_response(invoke())
            }
        };
    };
}

/// The exported `loom_schema` function: the guest's root `pub const
/// LOOM_SCHEMA: &str`, which the host appends only when the guest defines it.
#[doc(hidden)]
#[macro_export]
#[cfg_attr(target_arch = "wasm32", allow_internal_unsafe)]
macro_rules! __loom_export_schema {
    () => {
        const _: () = {
            #[unsafe(export_name = "loom_schema")]
            extern "C" fn schema() -> ::core::primitive::u64 {
                $crate::core::response(::std::result::Result::Ok(crate::LOOM_SCHEMA))
            }
        };
    };
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
