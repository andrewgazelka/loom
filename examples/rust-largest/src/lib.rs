#[loom::def(effects=["fs.list"])]
pub fn largest(machine: String, path: String) -> loom::Value {
    let listing = loom::fs::list(&machine, &path).expect("directory listing failed");
    let Some(entry) = listing
        .into_iter()
        .filter(|entry| entry.kind == loom::EntryKind::File)
        .max_by_key(|entry| entry.size)
    else {
        return loom::Value::Null;
    };
    loom::serde_json::json!({
        "name": entry.name,
        "size": entry.size,
        "is_dir": false,
        "is_file": true,
        "is_symlink": false,
    })
}
