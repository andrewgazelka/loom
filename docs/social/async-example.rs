use loom::{abilities::fs, EntryKind};

#[loom::def]
pub fn main(machine: String, path: String) -> Vec<String> {
    let entries = fs::walk(&machine, &path, 64, 10_000).unwrap();

    entries.into_iter()
        .filter(|entry| entry.kind == EntryKind::File)
        .map(|entry| entry.name)
        .collect()
}
