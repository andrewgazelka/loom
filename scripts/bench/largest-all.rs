#[loom::def]
pub fn main(machine: String, path: String) -> loom::Value {
    let mut frontier: Vec<String> = vec![String::new()];
    let mut best_path = String::new();
    let mut best_size: i64 = -1;
    while !frontier.is_empty() {
        let descs: Vec<loom::Desc<loom::Value>> = frontier
            .iter()
            .map(|rel| {
                let full = if rel.is_empty() {
                    path.clone()
                } else {
                    format!("{}/{}", path, rel)
                };
                loom::Desc::<loom::Value>::new(
                    "fs.list",
                    serde_json::json!({"machine":machine,"path":full}),
                )
            })
            .collect();
        let batches = loom::all(descs).expect("directory listing failed");
        let mut next: Vec<String> = Vec::new();
        for (index, batch) in batches.into_iter().enumerate() {
            let entries = batch.as_array().expect("fs.list must return an array");
            for entry in entries {
                if entry["is_symlink"].as_bool().expect("is_symlink") {
                    continue;
                }
                let name = entry["name"].as_str().expect("name");
                let rel = if frontier[index].is_empty() {
                    name.to_string()
                } else {
                    format!("{}/{}", frontier[index], name)
                };
                if entry["is_dir"].as_bool().expect("is_dir") {
                    next.push(rel);
                } else if entry["is_file"].as_bool().expect("is_file") {
                    let size = entry["size"].as_i64().expect("size");
                    if size > best_size || (size == best_size && rel < best_path) {
                        best_size = size;
                        best_path = rel;
                    }
                }
            }
        }
        frontier = next;
    }
    serde_json::json!({"path":best_path,"size":best_size})
}
