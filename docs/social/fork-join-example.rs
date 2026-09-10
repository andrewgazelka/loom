use loom::abilities::fs;

// Literal search of UTF-8 files. Paths are relative to the machine root.
#[loom::def]
pub fn main(machine: String, path: String, needle: String) -> Vec<String> {
    let entries = fs::list(&machine, &path).expect("directory listing failed");
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

    let jobs = directories.into_iter().map(|path| {
        loom::fork(MAIN_DEF, MainArgs {
            machine: machine.clone(), path, needle: needle.clone(),
        }).expect("fork failed")
    }).collect::<Vec<_>>();

    let mut matches = Vec::new();
    for file in files {
        if fs::read(&machine, &file).expect("read failed").contains(&needle) {
            matches.push(file);
        }
    }
    matches.extend(loom::join(jobs).expect("join failed")
        .into_iter().flatten());
    matches.sort();
    matches
}
