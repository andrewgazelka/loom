#[loom::def]
pub fn largest(machine: loom::Value, path: String) -> loom::Value {
    let listing = loom::abilities::fs::list(machine, &path).expect("directory listing failed");
    let entries = listing.as_array().expect("host returned non-array listing");
    entries
        .iter()
        .filter(|entry| entry["is_dir"] == false)
        .max_by_key(|entry| {
            entry["size"]
                .as_u64()
                .expect("host returned invalid file size")
        })
        .cloned()
        .unwrap_or(loom::Value::Null)
}
