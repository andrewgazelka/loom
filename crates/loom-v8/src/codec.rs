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
    let bytes = v8::Array::new(scope, json.len() as i32);
    for (index, byte) in json.bytes().enumerate() {
        let byte = v8::Integer::new_from_unsigned(scope, u32::from(byte));
        guest_ensure(
            bytes.set_index(scope, index as u32, byte.into()) == Some(true),
            "cannot allocate message bytes",
        )?;
    }
    output.set(bytes.into());
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
    let array = v8::Local::<v8::Array>::try_from(value)
        .map_err(|_| guest("Loom message must be an array of bytes"))?;
    guest_ensure(
        array.length() as usize <= execution::message_limit(scope),
        "Loom message exceeds byte limit",
    )?;
    let mut bytes = Vec::with_capacity(array.length() as usize);
    for index in 0..array.length() {
        let byte = array
            .get_index(scope, index)
            .ok_or_else(|| guest("cannot read message byte"))?;
        guest_ensure(byte.is_uint32(), "Loom message must contain integer bytes")?;
        let byte = byte
            .uint32_value(scope)
            .ok_or_else(|| guest("cannot read integer byte"))?;
        bytes.push(u8::try_from(byte).map_err(|_| guest("Loom message byte exceeds 255"))?);
    }
    let text = std::str::from_utf8(&bytes).map_err(|_| guest("Loom message is not valid UTF-8"))?;
    validate_json(text)?;
    let text =
        v8::String::new(scope, text).ok_or_else(|| guest("cannot allocate decoded message"))?;
    let value =
        v8::json::parse(scope, text).ok_or_else(|| guest("Loom message is not valid JSON"))?;
    output.set(value);
    Ok(())
}

// Apply the same nesting and exact-integer contract before JSON.parse can
// round a message integer. Validation does not rewrite JSON property order.
fn validate_json(text: &str) -> Result<()> {
    let value: serde_json::Value = serde_json::from_str(text)
        .map_err(|error| guest(format!("invalid Loom message JSON: {error}")))?;
    crate::validate_host_value(&value, 0)
}
