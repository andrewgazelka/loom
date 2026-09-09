//! Synchronous guest interface to the language-independent Loom host.
pub use loom_guest_macros::{actor, def};
pub use serde;
use serde::{Serialize, de::DeserializeOwned};
pub use serde_json;
use std::marker::PhantomData;

pub mod bindings {
    wit_bindgen::generate!({ path: "../../loom-wit", world: "handler", pub_export_macro: true, default_bindings_module: "::loom::bindings" });
}

pub type EffectError = String;
pub use loom_proto::{Desc, TypeSig, Value, decode, encode};

pub fn perform<T: DeserializeOwned>(desc: Desc<T>) -> Result<T, EffectError> {
    let bytes = encode(&desc)?;
    let result = bindings::loom::host::abilities::perform(&bytes)?;
    decode(&result)
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
#[derive(Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct Fiber<R> {
    pub id: Value,
    #[serde(skip)]
    result: PhantomData<fn() -> R>,
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
pub fn fork_desc<F: Invocation>(
    def: Def<F>,
    args: F::Args,
) -> Result<Desc<Fiber<F::Output>>, EffectError> {
    Ok(Desc::new(
        "fork",
        serde_json::json!({"def":def.hash,"args":F::arguments(args)?}),
    ))
}
pub fn fork<F: Invocation>(def: Def<F>, args: F::Args) -> Result<Fiber<F::Output>, EffectError> {
    perform(fork_desc(def, args)?)
}
pub fn join<R: DeserializeOwned>(
    fibers: impl IntoIterator<Item = Fiber<R>>,
) -> Result<Vec<R>, EffectError> {
    let ids: Vec<Value> = fibers.into_iter().map(|fiber| fiber.id).collect();
    perform(Desc::new("join", serde_json::json!({"fibers": ids})))
}
pub fn all<T: DeserializeOwned>(
    descs: impl IntoIterator<Item = Desc<T>>,
) -> Result<Vec<T>, EffectError> {
    let descs: Vec<Desc<T>> = descs.into_iter().collect();
    perform(Desc::new("all", serde_json::json!({"descs": descs})))
}
pub fn call_desc<F: Invocation>(
    def: Def<F>,
    args: F::Args,
) -> Result<Desc<F::Output>, EffectError> {
    Ok(Desc::new(
        "call",
        serde_json::json!({"def":def.hash,"args":F::arguments(args)?}),
    ))
}
pub fn call<F: Invocation>(def: Def<F>, args: F::Args) -> Result<F::Output, EffectError> {
    perform(call_desc(def, args)?)
}

pub trait Actor {
    type State: Serialize + DeserializeOwned;
    type Event: Serialize + DeserializeOwned;
    type Msg: Serialize + DeserializeOwned;
    fn init() -> Self::State;
    fn fold(state: Self::State, event: &Self::Event) -> Self::State;
    fn handle(state: &Self::State, msg: Self::Msg) -> Vec<Self::Event>;
}

pub mod abilities {
    use super::*;
    pub fn now() -> Result<Value, EffectError> {
        perform(Desc::new("now", Value::Null))
    }
    pub fn random() -> Result<f64, EffectError> {
        perform(Desc::new("random", Value::Null))
    }
    pub fn sleep(ms: u64) -> Result<Value, EffectError> {
        perform(Desc::new("sleep", serde_json::json!({"ms": ms})))
    }
    pub fn exec(args: Value) -> Result<Value, EffectError> {
        perform(Desc::new("exec", args))
    }
    pub fn llm(args: Value) -> Result<Value, EffectError> {
        perform(Desc::new("llm", args))
    }
    pub fn send(actor: &str, msg: Value) -> Result<Value, EffectError> {
        perform(Desc::new(
            "send",
            serde_json::json!({"actor": actor,"msg":msg}),
        ))
    }
    pub mod fs {
        use super::*;
        pub fn list(machine: Value, path: &str) -> Result<Value, EffectError> {
            perform(Desc::new(
                "fs.list",
                serde_json::json!({"machine":machine,"path":path}),
            ))
        }
        pub fn stat(machine: Value, path: &str) -> Result<Value, EffectError> {
            perform(Desc::new(
                "fs.stat",
                serde_json::json!({"machine":machine,"path":path}),
            ))
        }
        pub fn read(machine: Value, path: &str) -> Result<Value, EffectError> {
            perform(Desc::new(
                "fs.read",
                serde_json::json!({"machine":machine,"path":path}),
            ))
        }
        pub fn snapshot(machine: Value, path: &str) -> Result<Value, EffectError> {
            perform(Desc::new(
                "fs.snapshot",
                serde_json::json!({"machine":machine,"path":path}),
            ))
        }
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
        let def: Def<fn(Vec<i64>) -> i64> = Def::new("known-definition");
        let desc = call_desc(def, vec![1, 2]).unwrap();
        assert_eq!(desc.args["args"], serde_json::json!([[1, 2]]));
    }
}
