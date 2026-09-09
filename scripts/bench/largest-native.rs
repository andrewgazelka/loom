//! Native filesystem baseline. No Loom, serialization, effect log, or MCP.
use std::{
    fs, io,
    path::{Path, PathBuf},
    time::Instant,
};

#[derive(Default)]
struct Largest {
    path: String,
    size: Option<u64>,
    files: usize,
    directories: usize,
}

fn scan(root: &Path, relative: &Path, largest: &mut Largest) -> io::Result<()> {
    let mut entries = fs::read_dir(root.join(relative))?.collect::<io::Result<Vec<_>>>()?;
    entries.sort_unstable_by_key(|entry| entry.file_name());
    for entry in entries {
        let metadata = entry.metadata()?; // Same non-following metadata operation as Loom.
        let path = relative.join(entry.file_name());
        if metadata.is_dir() {
            largest.directories += 1;
            scan(root, &path, largest)?;
        } else if metadata.is_file() {
            largest.files += 1;
            let path = path
                .to_str()
                .ok_or_else(|| io::Error::other("non-UTF8 path"))?;
            if largest.size.is_none_or(|size| {
                metadata.len() > size || (metadata.len() == size && path < largest.path.as_str())
            }) {
                largest.path = path.to_owned();
                largest.size = Some(metadata.len());
            }
        }
    }
    Ok(())
}

fn main() -> io::Result<()> {
    let root = PathBuf::from(
        std::env::args()
            .nth(1)
            .ok_or_else(|| io::Error::other("fixture path required"))?,
    );
    let start = Instant::now();
    let mut largest = Largest::default();
    scan(&root, Path::new(""), &mut largest)?;
    // TSV keeps this standalone benchmark dependency-free; the runner validates every field.
    println!(
        "{}\t{}\t{}\t{}\t{}",
        start.elapsed().as_secs_f64() * 1000.0,
        largest.size.map_or(-1_i128, i128::from),
        largest.files,
        largest.directories,
        largest.path
    );
    Ok(())
}
