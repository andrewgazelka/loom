// Each subtree advances independently; join merges only its winning entry.
#[loom::def]
pub fn main(machine: String, path: String) -> loom::Value {
    let entries = loom::abilities::fs::list(&machine, &path)
        .expect("directory listing failed");
    let mut best = serde_json::json!({"path":"", "size":-1});
    let mut fibers = Vec::new();
    let mut directories = Vec::new();
    for entry in entries {
        let name = &entry.name;
        if entry.kind == loom::EntryKind::Directory {
            fibers.push(
                loom::fork(
                    MAIN_DEF,
                    MainArgs {
                        machine: machine.clone(),
                        path: format!("{path}/{name}"),
                    },
                )
                .expect("fork failed"),
            );
            directories.push(name.to_owned());
        } else if entry.kind == loom::EntryKind::File {
            merge(&mut best, name, i64::try_from(entry.size).expect("file size exceeds i64"));
        }
    }
    for (index, child) in loom::join(fibers)
        .expect("join failed")
        .into_iter()
        .enumerate()
    {
        let size = child["size"].as_i64().expect("child size");
        if size >= 0 {
            let relative = format!(
                "{}/{}",
                directories[index],
                child["path"].as_str().expect("child path")
            );
            merge(&mut best, &relative, size);
        }
    }
    best
}

fn merge(best: &mut loom::Value, path: &str, size: i64) {
    let previous = best["size"].as_i64().expect("best size");
    if size > previous || (size == previous && path < best["path"].as_str().expect("best path")) {
        *best = serde_json::json!({"path":path,"size":size});
    }
}
