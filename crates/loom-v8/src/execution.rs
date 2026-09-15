//! Thread-affine V8 ownership. Only JSON descriptors and replies cross threads.
use crate::{
    EffectRequest, Limits, Program, Result, control::Control, guest, guest_ensure, pool::Counters,
};
use serde_json::Value;
use std::{
    collections::VecDeque,
    ffi::c_void,
    sync::{Arc, atomic::Ordering, mpsc},
    time::Duration,
};
use tokio::sync::mpsc as async_mpsc;

struct PendingEffect {
    resolver: v8::Global<v8::PromiseResolver>,
    reply: mpsc::Receiver<Value>,
}

struct Bridge {
    effects: Option<async_mpsc::Sender<EffectRequest>>,
    pending: VecDeque<PendingEffect>,
    control: Arc<Control>,
    max_message_bytes: usize,
    max_pending_effects: usize,
}

struct HeapLimit {
    control: Arc<Control>,
    reserve_bytes: usize,
}

extern "C" fn heap_limit(data: *mut c_void, current: usize, initial: usize) -> usize {
    // The callback is removed before its boxed state is dropped; V8 invokes it
    // synchronously on the isolate's owning worker. No V8 objects are allocated.
    let state = unsafe { &*(data.cast::<HeapLimit>()) };
    state.control.fail("JavaScript heap limit exceeded");
    // V8 fatally aborts if the callback refuses growth. Reserve bounded space
    // for the interrupted allocation to unwind, then dispose the whole isolate.
    current.max(initial.saturating_add(state.reserve_bytes))
}

fn with_isolate<T>(
    limits: &Limits,
    control: &Arc<Control>,
    effects: Option<async_mpsc::Sender<EffectRequest>>,
    operation: impl FnOnce(&mut v8::PinScope) -> Result<T>,
) -> Result<T> {
    control.check()?;
    let params = v8::CreateParams::default()
        .heap_limits(0, limits.heap_bytes)
        .allow_atomics_wait(false);
    let mut heap = Box::new(HeapLimit {
        control: control.clone(),
        reserve_bytes: 16 * 1024 * 1024,
    });
    let mut isolate = v8::Isolate::new(params);
    isolate.set_microtasks_policy(v8::MicrotasksPolicy::Explicit);
    control.install(isolate.thread_safe_handle());
    isolate.add_near_heap_limit_callback(heap_limit, (&mut *heap as *mut HeapLimit).cast());
    isolate.set_slot(Bridge {
        effects,
        pending: VecDeque::new(),
        control: control.clone(),
        max_message_bytes: limits.max_message_bytes,
        max_pending_effects: limits.max_pending_effects,
    });
    let result = {
        v8::scope!(let scope, &mut isolate);
        let context = v8::Context::new(scope, Default::default());
        let scope = &mut v8::ContextScope::new(scope, context);
        v8::tc_scope!(let scope, scope);
        let result = bootstrap(scope).and_then(|()| operation(scope));
        // Prefer the termination reason; converting an exception after V8 has
        // terminated can itself fail and must never hide timeout or heap errors.
        match control.check() {
            Err(error) => Err(error),
            Ok(()) if scope.has_caught() => {
                let detail = scope
                    .exception()
                    .and_then(|exception| exception.to_string(scope))
                    .map(|message| bounded_string(scope, message, limits.max_message_bytes))
                    .transpose()?
                    .unwrap_or_else(|| "JavaScript exception".into());
                Err(guest(detail))
            }
            Ok(()) => result,
        }
    };
    // Global handles in the bridge must die while the isolate is still alive.
    isolate.remove_slot::<Bridge>();
    isolate.remove_near_heap_limit_callback(heap_limit, 0);
    control.finish();
    drop(isolate);
    drop(heap);
    result
}

fn bootstrap(scope: &mut v8::PinScope) -> Result<()> {
    let callback =
        v8::Function::new(scope, perform).ok_or_else(|| guest("cannot create effect bridge"))?;
    let key = string(scope, "__loomPerform")?;
    let global = scope.get_current_context().global(scope);
    guest_ensure(
        global.set(scope, key.into(), callback.into()) == Some(true),
        "cannot install effect bridge",
    )?;
    crate::codec::register(scope)?;
    let source = string(scope, include_str!("bootstrap.js"))?;
    let script = v8::Script::compile(scope, source, None)
        .ok_or_else(|| guest("cannot compile Loom bootstrap"))?;
    script
        .run(scope)
        .ok_or_else(|| guest("cannot initialize Loom bootstrap"))?;
    let source = string(scope, include_str!("api.js"))?;
    let script = v8::Script::compile(scope, source, None)
        .ok_or_else(|| guest("cannot compile Loom JavaScript API"))?;
    script
        .run(scope)
        .ok_or_else(|| guest("cannot initialize Loom JavaScript API"))?;
    Ok(())
}

fn wrapped_source(source: &str) -> String {
    // Do not inject lexical identifiers beside source declarations: ordinary
    // user names such as `schema` must remain available to their program.
    format!(
        "(function() {{\n'use strict';\n{source}\n;\nreturn {{main, schema: typeof LOOM_SCHEMA === 'undefined' ? '' : LOOM_SCHEMA}};\n}})()"
    )
}

pub(crate) fn compile(
    source: &str,
    limits: &Limits,
    control: &Arc<Control>,
    counters: &Counters,
) -> Result<Program> {
    let source = wrapped_source(source);
    with_isolate(limits, control, None, |scope| {
        let code = string(scope, &source)?;
        let mut compiler_source = v8::script_compiler::Source::new(code, None);
        let script = v8::script_compiler::compile_unbound_script(
            scope,
            &mut compiler_source,
            v8::script_compiler::CompileOptions::EagerCompile,
            v8::script_compiler::NoCacheReason::NoReason,
        )
        .ok_or_else(|| guest("JavaScript compilation failed"))?;
        let bound = script.bind_to_current_context(scope);
        let exports = bound
            .run(scope)
            .ok_or_else(|| guest("JavaScript initialization failed"))?;
        let exports = v8::Local::<v8::Object>::try_from(exports)
            .map_err(|_| guest("invalid JavaScript definition"))?;
        // Source may return early from its wrapper, so Rust must validate the
        // exported contract too; the JavaScript checks alone are bypassable.
        let main_key = string(scope, "main")?;
        let main = exports
            .get(scope, main_key.into())
            .ok_or_else(|| guest("missing JavaScript main"))?;
        let main = v8::Local::<v8::Function>::try_from(main)
            .map_err(|_| guest("JavaScript main must be a function"))?;
        let has_startup =
            crate::codec::lifecycle_handler(scope, main, crate::codec::Lifecycle::Startup)?
                .is_some();
        let has_shutdown =
            crate::codec::lifecycle_handler(scope, main, crate::codec::Lifecycle::Shutdown)?
                .is_some();
        let schema_key = string(scope, "schema")?;
        let schema = exports
            .get(scope, schema_key.into())
            .ok_or_else(|| guest("missing schema"))?;
        let schema = v8::Local::<v8::String>::try_from(schema)
            .map_err(|_| guest("LOOM_SCHEMA must be SQL text"))?;
        let schema = bounded_string(scope, schema, limits.max_message_bytes)?;
        // Top-level promise callbacks are initialization too. They may not
        // escape admission and invoke actor effects only on a later call.
        scope.perform_microtask_checkpoint();
        control.check()?;
        let code_cache = script
            .create_code_cache()
            .ok_or_else(|| guest("V8 did not produce compiled code cache"))?
            .to_vec();
        counters.compilations.fetch_add(1, Ordering::Relaxed);
        Ok(Program {
            source: source.clone(),
            code_cache,
            schema,
            has_startup,
            has_shutdown,
        })
    })
}

pub(crate) fn call(
    program: &Program,
    input: &crate::Input,
    effects: async_mpsc::Sender<EffectRequest>,
    limits: &Limits,
    control: &Arc<Control>,
    counters: &Counters,
    worker: &crate::pool::Worker,
    depth: usize,
) -> Result<Value> {
    with_isolate(limits, control, Some(effects), |scope| {
        let code = string(scope, &program.source)?;
        let mut source = v8::script_compiler::Source::new_with_cached_data(
            code,
            None,
            v8::CachedData::new(&program.code_cache),
        );
        let script = v8::script_compiler::compile(
            scope,
            &mut source,
            v8::script_compiler::CompileOptions::ConsumeCodeCache,
            v8::script_compiler::NoCacheReason::NoReason,
        )
        .ok_or_else(|| guest("JavaScript cached compilation failed"))?;
        // Programs never cross an engine-version boundary: rejection is an
        // invariant failure rather than a silent change in execution cost.
        guest_ensure(
            source
                .get_cached_data()
                .is_some_and(|cache| !cache.rejected()),
            "V8 rejected compiled program cache",
        )?;
        counters.cache_hits.fetch_add(1, Ordering::Relaxed);
        let exports = script
            .run(scope)
            .ok_or_else(|| guest("JavaScript initialization failed"))?;
        let exports = v8::Local::<v8::Object>::try_from(exports)
            .map_err(|_| guest("invalid JavaScript exports"))?;
        let key = string(scope, "main")?;
        let main = exports
            .get(scope, key.into())
            .ok_or_else(|| guest("missing JavaScript main"))?;
        let main = v8::Local::<v8::Function>::try_from(main)
            .map_err(|_| guest("JavaScript main must be a function"))?;
        let mut positional = Vec::new();
        let main = match input {
            crate::Input::Arguments { json: args } => {
                let args_json = string(scope, args)?;
                let args = v8::json::parse(scope, args_json)
                    .ok_or_else(|| guest("invalid argument JSON"))?;
                let args = v8::Local::<v8::Array>::try_from(args)
                    .map_err(|_| guest("arguments must be an array"))?;
                guest_ensure(
                    args.length() <= 65_535,
                    "too many JavaScript positional arguments",
                )?;
                for index in 0..args.length() {
                    positional.push(
                        args.get_index(scope, index)
                            .ok_or_else(|| guest("cannot read argument"))?,
                    );
                }
                main
            }
            crate::Input::Startup => {
                crate::codec::lifecycle_handler(scope, main, crate::codec::Lifecycle::Startup)?
                    .ok_or_else(|| guest("startup callback changed after compilation"))?
            }
            crate::Input::Shutdown { reason } => {
                positional.push(string(scope, reason)?.into());
                crate::codec::lifecycle_handler(scope, main, crate::codec::Lifecycle::Shutdown)?
                    .ok_or_else(|| guest("shutdown callback changed after compilation"))?
            }
            crate::Input::Message { bytes } => {
                guest_ensure(
                    bytes.len() <= limits.max_message_bytes,
                    "Loom message exceeds byte limit",
                )?;
                if let Some(handler) = crate::codec::json_handler(scope, main)? {
                    // The registered wrapper retains normal positional-call
                    // semantics. Actor calls can parse the original payload
                    // directly, avoiding decimal byte arrays entirely.
                    positional.push(crate::codec::decode_message(scope, bytes)?);
                    handler
                } else {
                    positional.push(crate::codec::byte_array(scope, bytes)?.into());
                    main
                }
            }
        };
        let receiver = v8::undefined(scope).into();
        let output = main
            .call(scope, receiver, &positional)
            .ok_or_else(|| guest("JavaScript main failed"))?;
        let resolver = v8::PromiseResolver::new(scope)
            .ok_or_else(|| guest("cannot create completion promise"))?;
        guest_ensure(
            resolver.resolve(scope, output) == Some(true),
            "cannot resolve completion promise",
        )?;
        let promise = resolver.get_promise(scope);
        drive(scope, promise, limits, control, worker, depth)
    })
}

fn drive(
    scope: &mut v8::PinScope,
    promise: v8::Local<v8::Promise>,
    limits: &Limits,
    control: &Control,
    worker: &crate::pool::Worker,
    depth: usize,
) -> Result<Value> {
    let mut serialized = None;
    loop {
        control.check()?;
        scope.perform_microtask_checkpoint();
        control.check()?;
        let pending = scope.get_slot_mut::<Bridge>().unwrap().pending.pop_front();
        if let Some(pending) = pending {
            let reply = loop {
                control.check()?;
                match pending.reply.recv_timeout(Duration::from_millis(5)) {
                    Ok(value) => break value,
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        worker.run_pending(depth);
                        continue;
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        return Err(guest("Loom effect caller stopped"));
                    }
                }
            };
            let reply = crate::encode_message(&reply, limits.max_message_bytes)?;
            let reply = string(scope, &reply)?;
            let reply = v8::json::parse(scope, reply)
                .ok_or_else(|| guest("invalid Loom effect response"))?;
            let resolver = v8::Local::new(scope, pending.resolver);
            guest_ensure(
                resolver.resolve(scope, reply) == Some(true),
                "cannot resolve Loom effect response",
            )?;
            continue;
        }
        if let Some(value) = serialized.take() {
            return Ok(value);
        }
        match promise.state() {
            v8::PromiseState::Pending => {
                return Err(guest("JavaScript promise is pending without a Loom effect"));
            }
            v8::PromiseState::Rejected => {
                let reason = promise
                    .result(scope)
                    .to_string(scope)
                    .ok_or_else(|| guest("JavaScript promise rejected"))?;
                return Err(guest(bounded_string(
                    scope,
                    reason,
                    limits.max_message_bytes,
                )?));
            }
            v8::PromiseState::Fulfilled => {
                // JSON.stringify can invoke user toJSON hooks that issue more
                // effects. Preserve the encoded result and drain their entire
                // continuation chain before crossing the transaction boundary.
                let output = promise.result(scope);
                serialized = Some(if output.is_undefined() {
                    // A JavaScript function without a return statement is the
                    // actor equivalent of a unit-returning Rust handler.
                    Value::Null
                } else {
                    decode(scope, output, limits.max_message_bytes)?
                });
            }
        };
    }
}

fn perform(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut output: v8::ReturnValue,
) {
    if let Err(error) = enqueue_effect(scope, args.get(0), &mut output) {
        // Throwing would let guest code catch a failed actor operation and
        // commit partial state. Termination is uncatchable and aborts the call.
        let control = scope.get_slot::<Bridge>().unwrap().control.clone();
        control.fail(format!("{error:#}"));
    }
}

fn enqueue_effect(
    scope: &mut v8::PinScope,
    descriptor: v8::Local<v8::Value>,
    output: &mut v8::ReturnValue,
) -> Result<()> {
    let bridge = scope.get_slot::<Bridge>().unwrap();
    bridge.control.check()?;
    let sender = bridge
        .effects
        .clone()
        .ok_or_else(|| guest("Loom effects are forbidden during definition initialization"))?;
    guest_ensure(
        bridge.pending.len() < bridge.max_pending_effects,
        "JavaScript pending effect limit exceeded",
    )?;
    let descriptor = effect_descriptor(scope, descriptor, bridge.max_message_bytes)?;
    guest_ensure(
        descriptor.is_object()
            && descriptor.get("op").is_some_and(Value::is_string)
            && descriptor.get("args").is_some(),
        "Loom effect requires op string and args",
    )?;
    // JSON serialization can invoke guest toJSON methods, which can enqueue
    // effects themselves. Recheck after that reentrant JavaScript finishes.
    let bridge = scope.get_slot::<Bridge>().unwrap();
    guest_ensure(
        bridge.pending.len() < bridge.max_pending_effects,
        "JavaScript pending effect limit exceeded",
    )?;
    let resolver =
        v8::PromiseResolver::new(scope).ok_or_else(|| guest("cannot create effect promise"))?;
    let promise = resolver.get_promise(scope);
    let resolver = v8::Global::new(scope, resolver);
    let (reply, receiver) = mpsc::sync_channel(1);
    sender
        .try_send(EffectRequest { descriptor, reply })
        .map_err(|_| guest("Loom effect queue is full or closed"))?;
    scope
        .get_slot_mut::<Bridge>()
        .unwrap()
        .pending
        .push_back(PendingEffect {
            resolver,
            reply: receiver,
        });
    output.set(promise.into());
    Ok(())
}

fn string<'s>(scope: &v8::PinScope<'s, '_>, value: &str) -> Result<v8::Local<'s, v8::String>> {
    v8::String::new(scope, value).ok_or_else(|| guest("cannot allocate JavaScript string"))
}

pub(crate) fn bounded_string(
    scope: &v8::PinScope,
    value: v8::Local<v8::String>,
    limit: usize,
) -> Result<String> {
    guest_ensure(
        value.utf8_length(scope) <= limit,
        "Loom message exceeds byte limit",
    )?;
    Ok(value.to_rust_string_lossy(scope))
}

pub(crate) fn decode(
    scope: &v8::PinScope,
    value: v8::Local<v8::Value>,
    limit: usize,
) -> Result<Value> {
    let json = v8::json::stringify(scope, value)
        .ok_or_else(|| guest("Loom values must be JSON serializable"))?;
    let json = bounded_string(scope, json, limit)?;
    let value = serde_json::from_str(&json)
        .map_err(|error| guest(format!("invalid Loom JSON: {error}")))?;
    crate::validate_host_value(&value, 0)?;
    Ok(value)
}

pub(crate) fn message_limit(scope: &v8::PinScope) -> usize {
    scope.get_slot::<Bridge>().unwrap().max_message_bytes
}

pub(crate) fn fail(scope: &mut v8::PinScope, error: anyhow::Error) {
    scope
        .get_slot::<Bridge>()
        .unwrap()
        .control
        .fail(format!("{error:#}"));
}

fn effect_descriptor(
    scope: &v8::PinScope,
    descriptor: v8::Local<v8::Value>,
    limit: usize,
) -> Result<Value> {
    let object = v8::Local::<v8::Object>::try_from(descriptor)
        .map_err(|_| guest("Loom effect requires an object"))?;
    let key = string(scope, "op")?;
    let operation = object
        .get(scope, key.into())
        .ok_or_else(|| guest("missing effect operation"))?;
    let operation = v8::Local::<v8::String>::try_from(operation)
        .map_err(|_| guest("Loom effect requires op string"))?;
    let operation = bounded_string(scope, operation, limit)?;
    let has_message = matches!(
        operation.as_str(),
        "actor.send" | "actor.call" | "actor.reply" | "actor.send_after"
    );
    if !has_message {
        return decode(scope, descriptor, limit);
    }
    // Each u8 occupies at most four JSON bytes including its comma. Account
    // for that transport expansion separately from the logical payload and
    // metadata budgets; no other effect receives this larger admission bound.
    let mut descriptor = decode(scope, descriptor, limit.saturating_mul(5))?;
    guest_ensure(
        descriptor.get("op").and_then(Value::as_str) == Some(operation.as_str()),
        "effect operation changed during serialization",
    )?;
    let message = descriptor
        .get_mut("args")
        .and_then(Value::as_object_mut)
        .and_then(|args| args.get_mut("msg"))
        .map(std::mem::take)
        .ok_or_else(|| guest("actor message effect requires msg bytes"))?;
    let bytes = message
        .as_array()
        .ok_or_else(|| guest("actor message must be an array of bytes"))?;
    guest_ensure(bytes.len() <= limit, "Loom message exceeds byte limit")?;
    guest_ensure(
        bytes
            .iter()
            .all(|byte| byte.as_u64().is_some_and(|byte| byte <= 255)),
        "actor message must contain integer bytes from 0 to 255",
    )?;
    // Temporarily replacing msg with null measures the remaining descriptor
    // without allocating or serializing its byte array for a second time.
    crate::encode_message(&descriptor, limit)?;
    descriptor["args"]["msg"] = message;
    Ok(descriptor)
}
