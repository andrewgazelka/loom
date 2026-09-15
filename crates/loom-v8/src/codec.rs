//! Native UTF-8 boundary for ordinary JSON actor messages.
use crate::{Result, execution, guest, guest_ensure};

pub(crate) fn register(scope: &mut v8::PinScope) -> Result<()> {
    let global = scope.get_current_context().global(scope);
    let encode =
        v8::Function::new(scope, encode).ok_or_else(|| guest("cannot create message encoder"))?;
    let key = v8::String::new(scope, "__loomEncode")
        .ok_or_else(|| guest("cannot allocate encoder key"))?;
    guest_ensure(
        global.set(scope, key.into(), encode.into()) == Some(true),
        "cannot install message encoder",
    )?;
    let decode =
        v8::Function::new(scope, decode).ok_or_else(|| guest("cannot create message decoder"))?;
    let key = v8::String::new(scope, "__loomDecode")
        .ok_or_else(|| guest("cannot allocate decoder key"))?;
    guest_ensure(
        global.set(scope, key.into(), decode.into()) == Some(true),
        "cannot install message decoder",
    )?;
    let json = v8::Function::new(scope, register_json_handler)
        .ok_or_else(|| guest("cannot create JSON handler bridge"))?;
    let key = v8::String::new(scope, "__loomJson")
        .ok_or_else(|| guest("cannot allocate JSON handler key"))?;
    guest_ensure(
        global.set(scope, key.into(), json.into()) == Some(true),
        "cannot install JSON handler bridge",
    )?;
    let actor = v8::Function::new(scope, register_actor_handler)
        .ok_or_else(|| guest("cannot create actor handler bridge"))?;
    let key = v8::String::new(scope, "__loomActor")
        .ok_or_else(|| guest("cannot allocate actor handler key"))?;
    guest_ensure(
        global.set(scope, key.into(), actor.into()) == Some(true),
        "cannot install actor handler bridge",
    )
}

fn encode(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut output: v8::ReturnValue,
) {
    if let Err(error) = encode_value(scope, args.get(0), &mut output) {
        execution::fail(scope, error);
    }
}

fn encode_value(
    scope: &mut v8::PinScope,
    value: v8::Local<v8::Value>,
    output: &mut v8::ReturnValue,
) -> Result<()> {
    // Admit only an actual JSON.stringify string, including when a toJSON
    // hook returns undefined. The native regression test caught undefined
    // being accepted through V8's C++ serialization path.
    let json = v8::Local::<v8::String>::try_from(value)
        .map_err(|_| guest("Loom messages must be JSON serializable"))?;
    let json = execution::bounded_string(scope, json, execution::message_limit(scope))?;
    validate_json(&json)?;
    output.set(byte_array(scope, json.as_bytes())?.into());
    Ok(())
}

fn decode(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut output: v8::ReturnValue,
) {
    if let Err(error) = decode_value(scope, args.get(0), &mut output) {
        execution::fail(scope, error);
    }
}

fn decode_value(
    scope: &mut v8::PinScope,
    value: v8::Local<v8::Value>,
    output: &mut v8::ReturnValue,
) -> Result<()> {
    output.set(decode_array(scope, value)?);
    Ok(())
}

fn decode_array<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    value: v8::Local<v8::Value>,
) -> Result<v8::Local<'s, v8::Value>> {
    let array = v8::Local::<v8::Array>::try_from(value)
        .map_err(|_| guest("Loom message must be an array of bytes"))?;
    let length = array.length();
    guest_ensure(
        length as usize <= execution::message_limit(scope),
        "Loom message exceeds byte limit",
    )?;
    let mut bytes = Vec::with_capacity(length as usize);
    for index in 0..length {
        let byte = array
            .get_index(scope, index)
            .ok_or_else(|| guest("cannot read message byte"))?;
        guest_ensure(byte.is_uint32(), "Loom message must contain integer bytes")?;
        let byte = byte
            .uint32_value(scope)
            .ok_or_else(|| guest("cannot read integer byte"))?;
        bytes.push(u8::try_from(byte).map_err(|_| guest("Loom message byte exceeds 255"))?);
    }
    decode_message(scope, &bytes)
}

pub(crate) fn decode_message<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    bytes: &[u8],
) -> Result<v8::Local<'s, v8::Value>> {
    guest_ensure(
        bytes.len() <= execution::message_limit(scope),
        "Loom message exceeds byte limit",
    )?;
    let text = std::str::from_utf8(bytes).map_err(|_| guest("Loom message is not valid UTF-8"))?;
    validate_json(text)?;
    let text =
        v8::String::new(scope, text).ok_or_else(|| guest("cannot allocate decoded message"))?;
    let value =
        v8::json::parse(scope, text).ok_or_else(|| guest("Loom message is not valid JSON"))?;
    Ok(value)
}

// Apply the same nesting and exact-integer contract before JSON.parse can
// round a message integer. Validation does not rewrite JSON property order.
fn validate_json(text: &str) -> Result<()> {
    let value: serde_json::Value = serde_json::from_str(text)
        .map_err(|error| guest(format!("invalid Loom message JSON: {error}")))?;
    crate::validate_host_value(&value, 0)
}

pub(crate) fn byte_array<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    bytes: &[u8],
) -> Result<v8::Local<'s, v8::Array>> {
    guest_ensure(
        bytes.len() <= execution::message_limit(scope),
        "Loom message exceeds byte limit",
    )?;
    let array = v8::Array::new(scope, bytes.len() as i32);
    for (index, byte) in bytes.iter().enumerate() {
        // Bound temporary V8 handles independently of payload length.
        v8::scope!(let inner, scope);
        let byte = v8::Integer::new_from_unsigned(inner, u32::from(*byte));
        guest_ensure(
            array.set_index(inner, index as u32, byte.into()) == Some(true),
            "cannot allocate message bytes",
        )?;
    }
    Ok(array)
}

fn handler_key<'s>(scope: &v8::PinScope<'s, '_>) -> Result<v8::Local<'s, v8::Private>> {
    let name = v8::String::new(scope, "loom.messages.json#handler")
        .ok_or_else(|| guest("cannot allocate JSON handler marker"))?;
    Ok(v8::Private::for_api(scope, Some(name)))
}

/// Only wrappers created by the native registration callback carry this V8
/// private property. Guest properties and symbols cannot forge the marker.
pub(crate) fn json_handler<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    main: v8::Local<v8::Function>,
) -> Result<Option<v8::Local<'s, v8::Function>>> {
    let key = handler_key(scope)?;
    let handler = main
        .get_private(scope, key)
        .ok_or_else(|| guest("cannot inspect JSON handler"))?;
    if handler.is_undefined() {
        return Ok(None);
    }
    Ok(Some(
        v8::Local::<v8::Function>::try_from(handler)
            .map_err(|_| guest("invalid JSON handler marker"))?,
    ))
}

fn register_json_handler(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut output: v8::ReturnValue,
) {
    let result = (|| -> Result<()> {
        let handler = v8::Local::<v8::Function>::try_from(args.get(0))
            .map_err(|_| guest("Message handler must be a function"))?;
        let wrapper = create_json_wrapper(scope, handler)?;
        output.set(wrapper.into());
        Ok(())
    })();
    if let Err(error) = result {
        execution::fail(scope, error);
    }
}

fn invoke_json_handler(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut output: v8::ReturnValue,
) {
    let handler = match v8::Local::<v8::Function>::try_from(args.data()) {
        Ok(handler) => handler,
        Err(_) => {
            execution::fail(scope, guest("invalid JSON handler"));
            return;
        }
    };
    let value = match decode_array(scope, args.get(0)) {
        Ok(value) => value,
        Err(error) => {
            execution::fail(scope, error);
            return;
        }
    };
    let receiver = v8::undefined(scope).into();
    // Keep guest exceptions catchable when a program invokes its wrapper
    // itself. Host/codec failures already terminate through their own paths.
    if let Some(result) = handler.call(scope, receiver, &[value]) {
        output.set(result);
    }
}

fn create_json_wrapper<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    handler: v8::Local<v8::Function>,
) -> Result<v8::Local<'s, v8::Function>> {
    let handler = v8::Local::new(scope, handler);
    let wrapper = v8::Function::builder(invoke_json_handler)
        .constructor_behavior(v8::ConstructorBehavior::Throw)
        .data(handler.into())
        .build(scope)
        .ok_or_else(|| guest("cannot allocate JSON handler wrapper"))?;
    let key = handler_key(scope)?;
    guest_ensure(
        wrapper.set_private(scope, key, handler.into()) == Some(true),
        "cannot mark JSON handler",
    )?;
    Ok(wrapper)
}

#[derive(Clone, Copy)]
pub(crate) enum Lifecycle {
    Startup,
    Shutdown,
}

fn lifecycle_key<'s>(
    scope: &v8::PinScope<'s, '_>,
    lifecycle: Lifecycle,
) -> Result<v8::Local<'s, v8::Private>> {
    let name = match lifecycle {
        Lifecycle::Startup => "loom.actor#startup",
        Lifecycle::Shutdown => "loom.actor#shutdown",
    };
    let name =
        v8::String::new(scope, name).ok_or_else(|| guest("cannot allocate lifecycle key"))?;
    Ok(v8::Private::for_api(scope, Some(name)))
}

pub(crate) fn lifecycle_handler<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    main: v8::Local<v8::Function>,
    lifecycle: Lifecycle,
) -> Result<Option<v8::Local<'s, v8::Function>>> {
    let key = lifecycle_key(scope, lifecycle)?;
    let value = main
        .get_private(scope, key)
        .ok_or_else(|| guest("cannot inspect lifecycle callback"))?;
    if value.is_undefined() {
        return Ok(None);
    }
    Ok(Some(
        v8::Local::<v8::Function>::try_from(value)
            .map_err(|_| guest("invalid lifecycle callback"))?,
    ))
}

fn register_actor_handler(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut output: v8::ReturnValue,
) {
    let result = (|| -> Result<()> {
        let message = v8::Local::<v8::Function>::try_from(args.get(0))
            .map_err(|_| guest("onMessage must be a function"))?;
        let wrapper = create_json_wrapper(scope, message)?;
        set_lifecycle(scope, wrapper, args.get(1), Lifecycle::Startup)?;
        set_lifecycle(scope, wrapper, args.get(2), Lifecycle::Shutdown)?;
        output.set(wrapper.into());
        Ok(())
    })();
    if let Err(error) = result {
        execution::fail(scope, error);
    }
}

fn set_lifecycle(
    scope: &mut v8::PinScope,
    wrapper: v8::Local<v8::Function>,
    value: v8::Local<v8::Value>,
    lifecycle: Lifecycle,
) -> Result<()> {
    if value.is_undefined() {
        return Ok(());
    }
    guest_ensure(value.is_function(), "lifecycle handler must be a function")?;
    let key = lifecycle_key(scope, lifecycle)?;
    guest_ensure(
        wrapper.set_private(scope, key, value) == Some(true),
        "cannot register lifecycle callback",
    )
}
