//! Isolated calls: run another definition in a fresh wasm instance.
//!
//! This is the isolation boundary: actor boundaries, untrusted code, another
//! language. It is NOT how code calls code. Code calls code by static linking
//! on the definition hash (definition dependencies compile as rlibs), which
//! gives ordinary typed Rust calls with no boundary at all.
//!
//! Untyped by design. `Def<F>` records the caller's belief about the callee's
//! signature; nothing checks it against the callee's stored signature until
//! the callee decodes the payload (a `CallError::Decode` names the mismatch)
//! or the host compares the declared arity with the stored export
//! (`CallError::Arity`, before instantiation). Typed checking against the
//! stored signature belongs to the static-linking path.
//!
//! Codec passes per call, counted at `loom_proto::isolated`: two for the
//! arguments (caller encodes, callee decodes) and two for the result (callee
//! encodes, caller decodes). The host parses a fixed header and one tag byte.
//! Nothing here goes through `serde_json::Value`; integers keep their 64-bit
//! range and `crate::Bytes` travels as a CBOR byte string.
//!
//! Guest handler frames (`loom::handle`) never see an isolated call: the
//! callee runs in its own memory and inherits no frames, and the call itself
//! is not a `perform`. Effect-row inference labels it `"call"`.
use loom_proto::isolated::{Request, Response};
pub use loom_proto::isolated::{CallError, MAX_DEPTH, Target, decode_payload, encode_payload};
use serde::{Serialize, de::DeserializeOwned};
use std::marker::PhantomData;

/// A callee named by hash (or `"$self"`), optionally by entry, with the
/// caller's belief about its signature as the `F` marker.
///
/// ```
/// use loom_guest_rs::isolated::Def;
/// const DESCEND: Def<fn(u32) -> u32> = Def::new("$self");
/// const ECHO: Def<fn(String, u64) -> String> = Def::new("$self").entry("echo");
/// ```
pub struct Def<F> {
    target: Target,
    entry: &'static str,
    marker: PhantomData<fn() -> F>,
}
impl<F> Clone for Def<F> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<F> Copy for Def<F> {}
impl<F> std::fmt::Debug for Def<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Def({}, entry {:?})", self.hash(), self.entry)
    }
}
impl<F> Def<F> {
    /// `"$self"` or 64 hex characters, parsed at compile time in a `const`
    /// item. Any other literal fails compilation there; at run time it panics
    /// (a trap), so run-time hashes go through `from_hex`.
    pub const fn new(hash: &'static str) -> Self {
        match Target::parse(hash) {
            Some(target) => Self {
                target,
                entry: "",
                marker: PhantomData,
            },
            None => panic!("loom::isolated::Def::new: expected 64 hex characters or \"$self\""),
        }
    }
    /// The definition this code is running in.
    pub const fn this() -> Self {
        Self {
            target: Target::This,
            entry: "",
            marker: PhantomData,
        }
    }
    pub const fn from_digest(digest: [u8; 32]) -> Self {
        Self {
            target: Target::Hash(digest),
            entry: "",
            marker: PhantomData,
        }
    }
    /// A hash known only at run time; the failure is a `CallError::Decode`.
    pub fn from_hex(hash: &str) -> Result<Self, CallError> {
        Ok(Self {
            target: Target::from_hex(hash)?,
            entry: "",
            marker: PhantomData,
        })
    }
    /// Select an export by name. Without it the callee must have one export.
    pub const fn entry(self, name: &'static str) -> Self {
        Self { entry: name, ..self }
    }
    pub const fn target(&self) -> Target {
        self.target
    }
    pub const fn entry_name(&self) -> &'static str {
        self.entry
    }
    /// `"$self"` or the lowercase hex hash, the store's definition hash form.
    pub fn hash(&self) -> String {
        self.target.label()
    }
}

/// How a `Def<F>` signature encodes its arguments: always one DAG-CBOR array
/// of `ARITY` elements, so `fn(Vec<i64>) -> R` sends `[[1, 2]]` and
/// `fn(A, B) -> R` sends `[a, b]`. Implemented for `fn(...) -> R` of arity
/// zero through eight; arity one takes `A` directly, others take a tuple.
pub trait Invocation {
    type Args;
    type Output: DeserializeOwned;
    const ARITY: u32;
    fn encode_args(args: Self::Args) -> Result<Vec<u8>, CallError>;
}
impl<R: DeserializeOwned> Invocation for fn() -> R {
    type Args = ();
    type Output = R;
    const ARITY: u32 = 0;
    fn encode_args(_none: ()) -> Result<Vec<u8>, CallError> {
        // `()` would be CBOR null; a zero-length array keeps the one shape.
        encode_payload::<[(); 0]>(&[])
    }
}
impl<A: Serialize, R: DeserializeOwned> Invocation for fn(A) -> R {
    type Args = A;
    type Output = R;
    const ARITY: u32 = 1;
    fn encode_args(args: A) -> Result<Vec<u8>, CallError> {
        encode_payload(&(args,))
    }
}
macro_rules! invocation {
    ($arity:literal; $($name:ident),+) => {
        impl<$($name: Serialize,)+ R: DeserializeOwned> Invocation for fn($($name),+) -> R {
            type Args = ($($name,)+);
            type Output = R;
            const ARITY: u32 = $arity;
            fn encode_args(args: Self::Args) -> Result<Vec<u8>, CallError> {
                encode_payload(&args)
            }
        }
    };
}
invocation!(2; A, B);
invocation!(3; A, B, C);
invocation!(4; A, B, C, D);
invocation!(5; A, B, C, D, E);
invocation!(6; A, B, C, D, E, G);
invocation!(7; A, B, C, D, E, G, H);
invocation!(8; A, B, C, D, E, G, H, I);

/// Run `def` in a fresh instance with `args`, suspending until it returns.
///
/// Arguments: pass 1 of 2 here (`encode_args`), pass 2 in the callee wrapper.
/// Result: pass 1 in the callee wrapper, pass 2 of 2 here (`decode_payload`).
pub fn call<F: Invocation>(def: Def<F>, args: F::Args) -> Result<F::Output, CallError> {
    let payload = F::encode_args(args)?;
    let frame = Request {
        target: def.target,
        entry: def.entry,
        argc: F::ARITY,
        payload: &payload,
    }
    .encode();
    let response = crate::core::isolated(&frame, &def.hash())?;
    match Response::parse(&response) {
        Ok(Ok(result)) => decode_payload(result),
        Ok(Err(error)) => Err(error),
        Err(message) => Err(CallError::Decode {
            message: format!("malformed isolated response frame: {message}"),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use loom_proto::Bytes;

    #[test]
    fn every_arity_encodes_one_array() {
        assert_eq!(
            <fn() -> u8 as Invocation>::encode_args(()).unwrap(),
            [0x80]
        );
        assert_eq!(
            <fn(Vec<i64>) -> i64 as Invocation>::encode_args(vec![1, 2]).unwrap(),
            [0x81, 0x82, 0x01, 0x02]
        );
        assert_eq!(
            <fn(u8, u8) -> () as Invocation>::encode_args((1, 2)).unwrap(),
            [0x82, 0x01, 0x02]
        );
        let eight = <fn(u8, u8, u8, u8, u8, u8, u8, u8) -> () as Invocation>::encode_args((
            1, 2, 3, 4, 5, 6, 7, 8,
        ))
        .unwrap();
        assert_eq!(eight, [0x88, 1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(<fn(u8, u8, u8, u8, u8, u8, u8, u8) -> () as Invocation>::ARITY, 8);
        let (max, blob): (u64, Bytes) =
            decode_payload(&<fn(u64, Bytes) -> () as Invocation>::encode_args((u64::MAX, Bytes::from(vec![9; 3]))).unwrap())
                .unwrap();
        assert_eq!(max, u64::MAX);
        assert_eq!(&*blob, &[9, 9, 9]);
    }

    #[test]
    fn def_is_const_and_carries_entry() {
        const THIS: Def<fn(u32) -> u32> = Def::new("$self").entry("descend");
        const HASH: Def<fn() -> ()> =
            Def::new("00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff");
        assert_eq!(THIS.target(), Target::This);
        assert_eq!(THIS.entry_name(), "descend");
        assert_eq!(THIS.hash(), "$self");
        assert_eq!(
            HASH.hash(),
            "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff"
        );
        assert!(matches!(
            Def::<fn() -> ()>::from_hex("not a hash"),
            Err(CallError::Decode { .. })
        ));
        let copied = THIS;
        assert_eq!(copied.entry_name(), THIS.entry_name());
    }

    #[test]
    fn call_outside_wasm_is_a_structured_error_not_a_trap() {
        const DEF: Def<fn(u8) -> u8> = Def::this();
        assert!(matches!(call(DEF, 1), Err(CallError::Trapped { .. })));
    }
}
