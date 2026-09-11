use loom::{fs, preview::{self, Preview}};

/// Both existing and new paths work. The file stays unchanged; the returned
/// value contains CAS-backed before/after content for Loom's diff viewer.
#[loom::def]
pub fn main(machine: String, path: String, content: String) -> Preview<String> {
    preview::writes(|| {
        fs::write(&machine, &path, "intermediate\n").expect("first write failed");
        fs::write(&machine, &path, &content).expect("write failed");
        fs::read(&machine, &path).expect("overlay read failed")
    }).expect("preview failed")
}
