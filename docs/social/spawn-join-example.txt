use loom::fs;

// Literal search of UTF-8 files. Paths are relative to the machine root.
pub fn main(machine: String, path: String, needle: String) -> Vec<String> {
    let mut matches = search(&machine, &path, &needle);
    matches.sort();
    matches
}

fn search(machine: &str, path: &str, needle: &str) -> Vec<String> {
    let entries = fs::list(machine, path).expect("directory listing failed");
    let mut directories = Vec::new();
    let mut files = Vec::new();
    for entry in entries {
        let child = format!("{path}/{}", entry.name);
        match entry.kind {
            loom::EntryKind::Directory => directories.push(child),
            loom::EntryKind::File => files.push(child),
            _ => {} // Never follow symlinks.
        }
    }

    loom::scope(|scope| {
        let jobs = directories.into_iter().map(|path| {
            scope.spawn(move || search(machine, &path, needle))
                .expect("spawn failed")
        }).collect::<Vec<_>>();

        let mut matches = files.into_iter().filter(|path| {
            fs::read(machine, path).expect("read failed").contains(needle)
        }).collect::<Vec<_>>();

        matches.extend(jobs.into_iter()
            .flat_map(|job| job.join().expect("join failed")));
        matches
    })
}
