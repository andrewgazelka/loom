#[loom::def]
pub fn main(machine: String, path: String) -> loom::Value {
    let mut frontier: Vec<String> = vec![String::new()];
    let mut best_path = String::new();
    let mut best_size: i64 = -1;
    while !frontier.is_empty() {
        let descs: Vec<loom::Desc<Vec<loom::DirEntry>>> = frontier
            .iter()
            .map(|rel| {
                let full = if rel.is_empty() {
                    path.clone()
                } else {
                    format!("{}/{}", path, rel)
                };
                loom::abilities::fs::list::desc(&machine, &full)
            })
            .collect();
        let batches = loom::all(descs).expect("directory listing failed");
        let mut next: Vec<String> = Vec::new();
        for (index, batch) in batches.into_iter().enumerate() {
            for entry in batch {
                let name = &entry.name;
                let rel = if frontier[index].is_empty() {
                    name.to_string()
                } else {
                    format!("{}/{}", frontier[index], name)
                };
                if entry.kind == loom::EntryKind::Directory {
                    next.push(rel);
                } else if entry.kind == loom::EntryKind::File {
                    let size = i64::try_from(entry.size).expect("file size exceeds i64");
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
