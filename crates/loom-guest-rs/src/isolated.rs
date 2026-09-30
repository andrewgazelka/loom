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
pub use loom_proto::isolated::{CallError, MAX_DEPTH, Target, decode_payload, encode_payload};
use loom_proto::isolated::{MAX_BATCH, Request, Response};
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
        Self {
            entry: name,
            ..self
        }
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

/// Run `def` once per element of `args` at the same time, and return the results in order.
///
/// The host runs the calls concurrently (about one per core at a time), each in its own
/// instance with the same semantics as [`call`]: policy, depth and arity are checked per call,
/// and a pure callee is answered from the result cache. Identical calls, in this batch or in
/// flight elsewhere, run once. Worth it when one call costs at least about a millisecond: each
/// is a fresh instantiation.
///
/// Anything that goes wrong with one call is that element's `Err`, at its position, and does not
/// stop the others: a callee failure, an argument that fails to encode (`CallError::Decode`), and
/// a result the batch has no room left for (the host returns at most 64 MiB of results per batch,
/// counted in argument order: the first result that does not fit and every later one report
/// `CallError::Trapped`).
/// The outer `Err` is only for a batch the host could not answer at all. More than
/// [`MAX_BATCH`] elements go to the host in consecutive batches of that size, so they are
/// concurrent within a batch, not across batches.
pub fn call_map<F: Invocation>(
    def: Def<F>,
    args: impl IntoIterator<Item = F::Args>,
) -> Result<Vec<Result<F::Output, CallError>>, CallError> {
    // One slot per element; an argument that does not encode is settled here, the rest (their
    // frames, and the slot each will fill) go to the host.
    let mut results: Vec<Option<Result<F::Output, CallError>>> = Vec::new();
    let mut frames = Vec::new();
    let mut positions = Vec::new();
    for (position, args) in args.into_iter().enumerate() {
        match F::encode_args(args) {
            Ok(payload) => {
                frames.push(
                    Request {
                        target: def.target,
                        entry: def.entry,
                        argc: F::ARITY,
                        payload: &payload,
                    }
                    .encode(),
                );
                positions.push(position);
                results.push(None);
            }
            Err(error) => results.push(Some(Err(error))),
        }
    }
    for (frames, positions) in frames.chunks(MAX_BATCH).zip(positions.chunks(MAX_BATCH)) {
        let response =
            crate::core::isolated_batch(&loom_proto::isolated::batch_frame(frames), &def.hash())?;
        let responses =
            loom_proto::isolated::parse_batch(&response).map_err(|message| CallError::Decode {
                message: format!("malformed isolated batch response: {message}"),
            })?;
        if responses.len() != frames.len() {
            return Err(CallError::Decode {
                message: format!(
                    "batch of {} calls answered with {}",
                    frames.len(),
                    responses.len()
                ),
            });
        }
        for (&position, frame) in positions.iter().zip(responses) {
            results[position] = Some(match Response::parse(frame) {
                Ok(Ok(result)) => decode_payload(result),
                Ok(Err(error)) => Err(error),
                Err(message) => Err(CallError::Decode {
                    message: format!("malformed isolated response frame: {message}"),
                }),
            });
        }
    }
    Ok(results
        .into_iter()
        .map(|result| result.expect("every element was either refused at encoding or answered"))
        .collect())
}

/// The `(start, end)` ranges [`parallel_for`] hands out: `0..n` in pieces of at most `chunk` (at least 1).
pub fn chunks(n: u32, chunk: u32) -> Vec<(u32, u32)> {
    let chunk = chunk.max(1);
    (0..n)
        .step_by(chunk as usize)
        .map(|start| (start, start.saturating_add(chunk).min(n)))
        .collect()
}

/// A parallel-for: run the entry `def` once per chunk of `0..n` (at most `chunk` indices each) at the same
/// time, and return the chunks' results in order. `def` takes `(start, end)`; a chunk that fails is that
/// chunk's `Err`. The host runs the chunks on its own threads (about one per core), each in a fresh instance
/// with its own memory, so a chunk sees only its arguments: pass what it needs (or a blob handle to read with
/// [`crate::kernel::get`]), and return its part of the result. Worth it when a chunk costs about a millisecond
/// or more, and pure chunks are answered from the result cache like any [`call_map`] element. Built on
/// [`call_map`], which this is a range-shaped front for; there are no guest threads.
pub fn parallel_for<R: DeserializeOwned>(
    def: Def<fn(u32, u32) -> R>,
    n: u32,
    chunk: u32,
) -> Result<Vec<Result<R, CallError>>, CallError> {
    let plan = chunk_plan(n, chunk)?;
    call_map(def, plan)
}

/// The most chunks one [`parallel_for`] creates. Each is a call with its own frame and instance; a range that
/// would need more is an error, not a million-instance fan-out. Raise `chunk` instead.
pub const MAX_CHUNKS: usize = 16 * 1024;

/// [`chunks`], refusing a plan of more than [`MAX_CHUNKS`] (a `chunk` of 0 counts as 1, so `n` itself must fit).
pub fn chunk_plan(n: u32, chunk: u32) -> Result<Vec<(u32, u32)>, CallError> {
    let size = chunk.max(1) as usize;
    let count = (n as usize).div_ceil(size);
    if count > MAX_CHUNKS {
        return Err(CallError::Decode {
            message: format!(
                "parallel_for({n}, chunk {chunk}) would make {count} chunks; the most is {MAX_CHUNKS}, so use a larger chunk"
            ),
        });
    }
    Ok(chunks(n, chunk))
}

#[cfg(test)]
mod tests {
    use super::*;
    use loom_proto::Bytes;

    #[test]
    fn a_plan_with_too_many_chunks_is_refused_and_an_empty_range_is_empty() {
        assert!(chunk_plan(0, 10).unwrap().is_empty());
        assert_eq!(chunk_plan(10, 4).unwrap().len(), 3);
        assert_eq!(chunk_plan(MAX_CHUNKS as u32, 1).unwrap().len(), MAX_CHUNKS);
        let error = chunk_plan(MAX_CHUNKS as u32 + 1, 1).unwrap_err();
        assert!(matches!(&error, CallError::Decode { message } if message.contains("larger chunk")), "{error:?}");
        assert!(chunk_plan(1_000_000, 0).is_err(), "a chunk of 0 is 1, which is too many for a million");
        assert_eq!(chunk_plan(u32::MAX, u32::MAX).unwrap(), [(0, u32::MAX)]);
    }

    #[test]
    fn chunks_cover_the_range_once_in_order() {
        assert_eq!(chunks(10, 4), [(0, 4), (4, 8), (8, 10)]);
        assert_eq!(chunks(8, 4), [(0, 4), (4, 8)]);
        assert_eq!(chunks(0, 4), []);
        assert_eq!(chunks(3, 0), [(0, 1), (1, 2), (2, 3)], "a chunk of 0 means 1");
        assert_eq!(chunks(5, 100), [(0, 5)]);
        assert_eq!(chunks(u32::MAX, u32::MAX), [(0, u32::MAX)], "no overflow at the top");
    }

    #[test]
    fn every_arity_encodes_one_array() {
        assert_eq!(<fn() -> u8 as Invocation>::encode_args(()).unwrap(), [0x80]);
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
        assert_eq!(
            <fn(u8, u8, u8, u8, u8, u8, u8, u8) -> () as Invocation>::ARITY,
            8
        );
        let (max, blob): (u64, Bytes) = decode_payload(
            &<fn(u64, Bytes) -> () as Invocation>::encode_args((u64::MAX, Bytes::from(vec![9; 3])))
                .unwrap(),
        )
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
    fn an_argument_that_fails_to_encode_is_that_elements_error_not_the_whole_calls() {
        struct Unencodable;
        impl Serialize for Unencodable {
            fn serialize<S: serde::Serializer>(&self, _: S) -> Result<S::Ok, S::Error> {
                Err(serde::ser::Error::custom("refused"))
            }
        }
        const DEF: Def<fn(Unencodable) -> u8> = Def::this();
        // Nothing encodes, so nothing is sent to the host (which this native build has none of).
        let results = call_map(DEF, [Unencodable, Unencodable, Unencodable]).unwrap();
        assert_eq!(results.len(), 3);
        assert!(
            results
                .iter()
                .all(|result| matches!(result, Err(CallError::Decode { .. })))
        );
        assert!(call_map(DEF, std::iter::empty()).unwrap().is_empty());
    }

    #[test]
    fn call_outside_wasm_is_a_structured_error_not_a_trap() {
        const DEF: Def<fn(u8) -> u8> = Def::this();
        assert!(matches!(call(DEF, 1), Err(CallError::Trapped { .. })));
    }
}
