struct Winner {
    path: String,
    size: i64,
}

pub fn main(machine: String, path: String) -> loom::Value {
    let winner = scan(&machine, &path);
    let mut result = loom::serde_json::Map::new();
    result.insert("path".into(), winner.path.into());
    result.insert("size".into(), winner.size.into());
    loom::Value::Object(result)
}

fn scan(machine: &str, path: &str) -> Winner {
    let entries = loom::fs::list(machine, path)
        .expect("directory listing failed");
    loom::scope(|scope| {
        let mut best = Winner { path: String::new(), size: -1 };
        let mut jobs = Vec::new();
        for entry in entries {
            match entry.kind {
                loom::EntryKind::Directory => {
                    let name = entry.name;
                    let full = [path, "/", &name].concat();
                    jobs.push(scope.spawn(move || {
                        let mut child = scan(machine, &full);
                        if child.size >= 0 {
                            child.path = [&name, "/", &child.path].concat();
                        }
                        child
                    }).expect("spawn failed"));
                }
                loom::EntryKind::File => merge(
                    &mut best,
                    &entry.name,
                    i64::try_from(entry.size).expect("file size exceeds i64"),
                ),
                _ => {}
            }
        }
        for job in jobs {
            let child = job.join().expect("join failed");
            if child.size >= 0 {
                merge(&mut best, &child.path, child.size);
            }
        }
        best
    })
}

fn merge(best: &mut Winner, path: &str, size: i64) {
    if size > best.size || (size == best.size && path < best.path.as_str()) {
        best.path = path.into();
        best.size = size;
    }
}
