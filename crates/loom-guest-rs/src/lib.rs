//! Synchronous guest interface to the language-independent Loom host.
#[cfg(loom_core)]
#[doc(hidden)]
pub mod core;
#[cfg(any(loom_core, not(target_arch = "wasm32")))]
mod detached;
#[cfg(any(loom_core, not(target_arch = "wasm32")))]
mod scoped;
#[cfg(any(loom_core, not(target_arch = "wasm32")))]
pub use detached::{JoinHandle, spawn};
#[cfg(any(loom_core, not(target_arch = "wasm32")))]
pub use scoped::{Scope, ScopedJoinHandle, scope};
#[cfg(any(loom_core, not(target_arch = "wasm32")))]
mod handlers;
#[cfg(any(loom_core, not(target_arch = "wasm32")))]
pub mod preview;
#[cfg(any(loom_core, not(target_arch = "wasm32")))]
pub use handlers::{Continuation, Effect, Reply, handle, handle_any};
pub use loom_guest_macros::{actor, def, schema};
pub use serde;
use serde::{Serialize, de::DeserializeOwned};
pub use serde_json;
use std::marker::PhantomData;

#[cfg(not(loom_core))]
pub mod bindings {
    wit_bindgen::generate!({ path: "../../wit", world: "handler", pub_export_macro: true, default_bindings_module: "::loom::bindings" });
}

#[cfg(loom_core)]
pub mod bindings {
    pub use crate::core::Guest;
    pub use crate::export_core as export;
}

pub type EffectError = String;
pub use loom_proto::{DirEntry, EntryKind, TypeSig, Value, decode, decode_host, encode};

/// Perform an effect and decode its result. The call suspends until it completes.
pub fn perform<T: DeserializeOwned>(name: &str, args: impl Serialize) -> Result<T, EffectError> {
    let args = serde_json::to_value(args).map_err(|error| error.to_string())?;
    let desc = loom_proto::Desc::<T>::new(name, args);
    let bytes = encode(&desc)?;
    #[cfg(not(loom_core))]
    {
        let result = bindings::loom::host::effects::perform(&bytes)?;
        decode_host(&result)
    }
    #[cfg(loom_core)]
    {
        core::perform(&bytes)
    }
}

pub struct Def<F> {
    pub hash: &'static str,
    marker: PhantomData<F>,
}
impl<F> Def<F> {
    pub const fn new(hash: &'static str) -> Self {
        Self {
            hash,
            marker: PhantomData,
        }
    }
}
/// Typed argument encoding at the positional guest protocol boundary.
pub trait Invocation {
    type Args;
    type Output: DeserializeOwned;
    fn arguments(args: Self::Args) -> Result<Vec<Value>, EffectError>;
}
impl<A: Serialize, R: DeserializeOwned> Invocation for fn(A) -> R {
    type Args = A;
    type Output = R;
    fn arguments(args: A) -> Result<Vec<Value>, EffectError> {
        Ok(vec![
            serde_json::to_value(args).map_err(|error| error.to_string())?,
        ])
    }
}
/// Call another definition synchronously with typed positional arguments.
pub fn call<F: Invocation>(def: Def<F>, args: F::Args) -> Result<F::Output, EffectError> {
    perform(
        "call",
        serde_json::json!({"def":def.hash,"args":F::arguments(args)?}),
    )
}

pub trait Actor {
    type State: Serialize + DeserializeOwned;
    type Event: Serialize + DeserializeOwned;
    type Msg: Serialize + DeserializeOwned;
    fn init() -> Self::State;
    fn fold(state: Self::State, event: &Self::Event) -> Self::State;
    fn handle(state: &Self::State, msg: Self::Msg) -> Vec<Self::Event>;
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
pub mod actor {
    use super::{Def, EffectError, Invocation, Value, perform};

    pub fn send(actor: &str, msg: Value) -> Result<Value, EffectError> {
        perform("actor.send", serde_json::json!({"actor":actor,"msg":msg}))
    }

    pub fn spawn<F: Invocation>(def: Def<F>, state: Value) -> Result<Value, EffectError> {
        perform(
            "actor.spawn",
            serde_json::json!({"def":def.hash,"state":state}),
        )
    }
}
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
    #[test]
    fn unary_array_remains_one_argument() {
        let args = <fn(Vec<i64>) -> i64 as Invocation>::arguments(vec![1, 2]).unwrap();
        assert_eq!(args, vec![serde_json::json!([1, 2])]);
    }
}
