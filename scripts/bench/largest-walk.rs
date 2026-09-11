#[loom::def(effects=["fs.walk"])]
pub fn main(machine: String, path: String) -> loom::Value {
    let entries = loom::fs::walk(&machine, &path, 256, 100_000)
        .expect("tree walk failed");
    let mut best_path = String::new();
    let mut best_size: i64 = -1;
    for entry in entries {
        if entry.kind == loom::EntryKind::File {
            let size = i64::try_from(entry.size).expect("file size exceeds i64");
            if size > best_size || (size == best_size && entry.name < best_path) {
                best_path = entry.name;
                best_size = size;
            }
        }
    }
    serde_json::json!({"path":best_path,"size":best_size})
}
