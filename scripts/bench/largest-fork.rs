// Each subtree advances independently; join merges only its winning entry.
#[loom::def]
pub fn main(machine: String, path: String) -> loom::Value {
    let entries: loom::Value = loom::perform(loom::Desc::new(
        "fs.list",
        serde_json::json!({"machine": machine, "path": path}),
    ))
    .expect("directory listing failed");
    let mut best = serde_json::json!({"path":"", "size":-1});
    let mut fibers = Vec::new();
    let mut directories = Vec::new();
    for entry in entries.as_array().expect("directory entries") {
        if entry["is_symlink"].as_bool().expect("symlink flag") {
            continue;
        }
        let name = entry["name"].as_str().expect("entry name");
        if entry["is_dir"].as_bool().expect("directory flag") {
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
        } else if entry["is_file"].as_bool().expect("file flag") {
            merge(&mut best, name, entry["size"].as_i64().expect("file size"));
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
