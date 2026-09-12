//! One-time conversion of the v0 string signatures. Immutable historical events
//! retain their original bytes; migration events carry their typed replacement.
use anyhow::{Context, Result};
use loom_proto::{ExportSig, ParamSig, TypeSig, Value, ValueShape};
use rusqlite::{Connection, params};
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacySignature {
    #[serde(default)]
    exports: Vec<LegacyExport>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyExport {
    name: String,
    params: Vec<LegacyParam>,
    returns: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyParam {
    name: String,
    #[serde(rename = "type")]
    shape: String,
}
struct StoredSignature {
    hash: String,
    sig: String,
}

pub(super) fn run(connection: &mut Connection) -> Result<()> {
    let tx = connection.transaction()?;
    let policy_exists: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('defs') WHERE name='allowed_effects')",
        [],
        |r| r.get(0),
    )?;
    if !policy_exists {
        tx.execute_batch("ALTER TABLE defs ADD COLUMN allowed_effects TEXT;")?;
    }
    let definitions: Vec<StoredSignature> = {
        let mut q = tx.prepare("SELECT hash,type_sig FROM defs")?;
        q.query_map([], |r| {
            Ok(StoredSignature {
                hash: r.get(0)?,
                sig: r.get(1)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?
    };
    for definition in definitions {
        if serde_json::from_str::<TypeSig>(&definition.sig).is_ok() {
            continue;
        }
        let legacy: LegacySignature = serde_json::from_str(&definition.sig)
            .with_context(|| format!("unrecognized persisted signature {}", definition.hash))?;
        let typed = TypeSig {
            effects: Default::default(),
            exports: legacy
                .exports
                .into_iter()
                .map(|export| ExportSig {
                    name: export.name,
                    params: export
                        .params
                        .into_iter()
                        .map(|param| ParamSig {
                            name: param.name,
                            shape: shape(&param.shape),
                        })
                        .collect(),
                    returns: shape(&export.returns),
                    effects: Default::default(),
                })
                .collect(),
        };
        let event = serde_json::json!({"type":"definition_signature_migrated","version":1,"hash":definition.hash,"sig":typed});
        super::record_definition_event(&tx, &event)?;
        tx.execute(
            "UPDATE defs SET type_sig=? WHERE hash=?",
            params![serde_json::to_string(&typed)?, definition.hash],
        )?;
    }
    tx.execute("INSERT OR IGNORE INTO def_effects SELECT json_extract(bytes,'$.def_hash'),json_extract(bytes,'$.op') FROM definition_events WHERE json_extract(bytes,'$.type')='effect_invoked' AND json_type(bytes,'$.def_hash')='text'",[])?;
    tx.commit()?;
    Ok(())
}
fn shape(value: &str) -> ValueShape {
    let value = value.trim();
    if let Some(items) = value.strip_suffix("[]") {
        return ValueShape::Array {
            items: Box::new(shape(items)),
        };
    }
    if let Some(target) = value.strip_prefix("Ref<").and_then(|v| v.strip_suffix('>')) {
        return ValueShape::Ref {
            target: Box::new(shape(target)),
        };
    }
    match value {
        "null" | "void" | "undefined" | "()" => ValueShape::Null,
        "boolean" | "bool" => ValueShape::Boolean,
        "number" | "u8" | "u16" | "u32" | "u64" | "i8" | "i16" | "i32" | "i64" | "f32" | "f64" => {
            ValueShape::Number
        }
        "string" | "String" | "&str" => ValueShape::String,
        // The old checker serialized language-specific types with no structural
        // schema. Their only sound common boundary is the unrestricted Value.
        _ => ValueShape::Value,
    }
}
pub(super) fn replacements(
    events: &[loom_proto::Event],
) -> Result<std::collections::BTreeMap<String, Value>> {
    let mut replacements = std::collections::BTreeMap::new();
    for event in events {
        if event.event["type"] == "definition_signature_migrated" {
            let hash = event.event["hash"]
                .as_str()
                .context("signature migration missing hash")?;
            let sig: TypeSig = serde_json::from_value(event.event["sig"].clone())?;
            replacements.insert(hash.into(), serde_json::to_value(sig)?);
        }
    }
    Ok(replacements)
}
