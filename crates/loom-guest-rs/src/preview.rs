//! A guest handler that previews UTF-8 writes. Other effects remain real.
//! Reads and CAS writes made by this handler dispatch to the outer context;
//! only those root effects are recorded. This is not an exec sandbox.
use crate::{fs, EffectError, Reply, Value};
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub struct FilesystemChange {
    pub machine: String,
    pub path: String,
    pub before: Option<String>,
    pub after: Option<String>,
    pub captured: bool,
    pub content_encoding: String,
}
#[derive(Debug, Serialize, Deserialize)]
pub struct FilesystemCapture { pub scope: String, pub preview: bool }
#[derive(Debug, Serialize, Deserialize)]
pub struct Preview<R> {
    pub result: R,
    pub filesystem_changes: Vec<FilesystemChange>,
    pub filesystem_capture: FilesystemCapture,
}
#[derive(Deserialize)]
struct FileArgs { machine: String, path: String, #[serde(default)] content: Option<String> }
struct Pending { change: FilesystemChange, content: String }

// Match the machine root's path normalization so aliases share one overlay.
fn normalized_path(path: &str) -> Result<String, EffectError> {
    if path.len() > 4096 || path.contains('\0') {
        return Err("invalid machine path".into());
    }
    let mut parts = Vec::new();
    for part in path.split('/') {
        if part == ".." { return Err("parent traversal is forbidden".into()); }
        if !part.is_empty() && part != "." { parts.push(part); }
    }
    Ok(if parts.is_empty() { ".".into() } else { parts.join("/") })
}
fn capture(content: &str) -> Result<String, EffectError> {
    let reference: Value = crate::perform("cas.put", Value::String(content.into()))?;
    reference.get("$ref").and_then(Value::as_str).map(str::to_owned)
        .ok_or_else(|| "cas.put returned no content reference".into())
}
fn dispatch(label: &str, mut args: FileArgs, pending: &mut Vec<Pending>) -> Result<Value, EffectError> {
    args.path = normalized_path(&args.path)?;
    let existing = pending.iter().position(|item| item.change.machine == args.machine && item.change.path == args.path);
    match label {
        "fs.write" => {
            let content = args.content.ok_or_else(|| "fs.write requires string content".to_owned())?;
            if content.len() > 64 * 1024 * 1024 { return Err("file exceeds 64 MiB write limit".into()); }
            let after = capture(&content)?;
            if let Some(index) = existing {
                pending[index].change.after = Some(after);
                pending[index].content = content;
            } else {
                // This validates the pinned-root path even for a new file.
                let before = fs::read_optional(&args.machine, &args.path)?.as_deref().map(capture).transpose()?;
                pending.push(Pending { change: FilesystemChange {
                    machine: args.machine, path: args.path, before, after: Some(after),
                    captured: true, content_encoding: "dag-cbor-string".into(),
                }, content });
            }
            Ok(Value::Null)
        }
        "fs.read" | "fs.read_optional" => {
            if let Some(index) = existing { return Ok(Value::String(pending[index].content.clone())); }
            crate::perform(label, serde_json::json!({"machine":args.machine,"path":args.path}))
        }
        _ => unreachable!("label-selected preview handler"),
    }
}
/// Preview writes in `body`, including writes from inherited scoped children.
/// Repeated writes collapse to one before/final-after diff; subsequent reads
/// observe the previewed content. Calls into another definition do not inherit
/// this handler, and effects other than these filesystem effects stay real.
pub fn writes<F, R>(body: F) -> Result<Preview<R>, EffectError>
where F: FnOnce() -> R {
    let mut pending = Vec::new();
    let result = crate::handle(["fs.write", "fs.read", "fs.read_optional"], |op, _continuation| {
        let args = op.arg::<FileArgs>().expect("invalid preview filesystem arguments");
        Reply::Resume(dispatch(&op.name, args, &mut pending).expect("filesystem preview failed"))
    }, body)?;
    Ok(Preview {
        result,
        filesystem_changes: pending.into_iter().map(|item| item.change)
            .filter(|change| change.before != change.after).collect(),
        filesystem_capture: FilesystemCapture { scope: "preview".into(), preview: true },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn path_aliases_cannot_split_the_preview_overlay() {
        assert_eq!(normalized_path("./dir//file").unwrap(), "dir/file");
        assert_eq!(normalized_path("/dir/file").unwrap(), "dir/file");
        assert!(normalized_path("dir/../file").is_err());
        assert!(normalized_path("file\0suffix").is_err());
        assert!(normalized_path(&"x".repeat(4097)).is_err());
    }
}
