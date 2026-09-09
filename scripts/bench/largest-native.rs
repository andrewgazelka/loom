//! Native filesystem baseline. No Loom, serialization, effect log, or MCP.
use std::{
    collections::VecDeque,
    fs, io,
    path::{Path, PathBuf},
    sync::{Condvar, Mutex},
    time::Instant,
};

#[derive(Default)]
struct Largest {
    path: String,
    size: Option<u64>,
    files: usize,
    directories: usize,
}

struct PendingDirectories {
    paths: VecDeque<PathBuf>,
    active: usize,
    failed: bool,
}

struct DirectoryQueue {
    pending: Mutex<PendingDirectories>,
    changed: Condvar,
}

fn scan_parallel(root: &Path, workers: usize) -> io::Result<Largest> {
    let queue = DirectoryQueue {
        pending: Mutex::new(PendingDirectories {
            paths: VecDeque::from([PathBuf::new()]),
            active: 0,
            failed: false,
        }),
        changed: Condvar::new(),
    };
    std::thread::scope(|scope| {
        let mut handles = Vec::new();
        for _ in 0..workers {
            handles.push(scope.spawn(|| -> io::Result<Largest> {
                let mut largest = Largest::default();
                loop {
                    let relative = {
                        let mut pending = queue
                            .pending
                            .lock()
                            .map_err(|_| io::Error::other("directory queue poisoned"))?;
                        while pending.paths.is_empty() && pending.active > 0 && !pending.failed {
                            pending = queue
                                .changed
                                .wait(pending)
                                .map_err(|_| io::Error::other("directory queue poisoned"))?;
                        }
                        if pending.failed || pending.paths.is_empty() {
                            return Ok(largest);
                        }
                        pending.active += 1;
                        pending.paths.pop_front().expect("nonempty queue checked")
                    };
                    let result = (|| -> io::Result<Vec<PathBuf>> {
                        let mut children = Vec::new();
                        let mut entries =
                            fs::read_dir(root.join(&relative))?.collect::<io::Result<Vec<_>>>()?;
                        entries.sort_unstable_by_key(|entry| entry.file_name());
                        for entry in entries {
                            let metadata = entry.metadata()?;
                            let path = relative.join(entry.file_name());
                            if metadata.is_dir() {
                                largest.directories += 1;
                                children.push(path);
                            } else if metadata.is_file() {
                                largest.files += 1;
                                let path = path
                                    .to_str()
                                    .ok_or_else(|| io::Error::other("non-UTF8 path"))?;
                                if largest.size.is_none_or(|size| {
                                    metadata.len() > size
                                        || (metadata.len() == size && path < largest.path.as_str())
                                }) {
                                    largest.path = path.to_owned();
                                    largest.size = Some(metadata.len());
                                }
                            }
                        }
                        Ok(children)
                    })();
                    let mut pending = queue
                        .pending
                        .lock()
                        .map_err(|_| io::Error::other("directory queue poisoned"))?;
                    pending.active -= 1;
                    match result {
                        Ok(children) => pending.paths.extend(children),
                        Err(error) => {
                            pending.failed = true;
                            queue.changed.notify_all();
                            return Err(error);
                        }
                    }
                    queue.changed.notify_all();
                }
            }));
        }
        let mut largest = Largest::default();
        for handle in handles {
            let part = handle
                .join()
                .map_err(|_| io::Error::other("scan worker panicked"))??;
            largest.files += part.files;
            largest.directories += part.directories;
            if let Some(size) = part.size {
                if largest.size.is_none_or(|current| {
                    size > current || (size == current && part.path < largest.path)
                }) {
                    largest.path = part.path;
                    largest.size = Some(size);
                }
            }
        }
        Ok(largest)
    })
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
    let mut arguments = std::env::args().skip(1);
    let root = PathBuf::from(
        arguments
            .next()
            .ok_or_else(|| io::Error::other("fixture path required"))?,
    );
    let workers = arguments
        .next()
        .map(|value| value.parse::<usize>().map_err(io::Error::other))
        .transpose()?
        .unwrap_or(1);
    if !(1..=128).contains(&workers) || arguments.next().is_some() {
        return Err(io::Error::other(
            "usage: largest-native FIXTURE [WORKERS:1..128]",
        ));
    }
    let start = Instant::now();
    let largest = if workers == 1 {
        let mut largest = Largest::default();
        scan(&root, Path::new(""), &mut largest)?;
        largest
    } else {
        scan_parallel(&root, workers)?
    };
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
