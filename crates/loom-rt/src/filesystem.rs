//! Machine filesystem authority is an open directory, never a re-resolved pathname.
use anyhow::{Context, Result, ensure};
use loom_proto::{DirEntry, EntryKind};
use serde::{Deserialize, Serialize};
use std::{
    collections::VecDeque,
    ffi::CString,
    fs::File,
    io::{Read, Write},
    os::fd::{AsRawFd, FromRawFd, RawFd},
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::Arc,
};

#[cfg(target_os = "macos")]
mod darwin;
#[cfg(target_os = "linux")]
mod linux;

pub(crate) const READ_LIMIT: u64 = 64 * 1024 * 1024;
const RESULT_LIMIT: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RootIdentity {
    pub device: u64,
    pub inode: u64,
}

pub(crate) struct PinnedRoot {
    directory: File,
    pub path: PathBuf,
    pub identity: RootIdentity,
}

struct FileTarget {
    parent: File,
    name: String,
}

#[derive(Clone, Copy)]
pub(crate) struct WalkLimits {
    pub max_depth: u32,
    pub max_entries: usize,
}

impl PinnedRoot {
    pub fn open(path: &Path) -> Result<Self> {
        let path = path.canonicalize()?;
        let name = CString::new(path.as_os_str().as_encoded_bytes())?;
        // SAFETY: name is NUL terminated; open returns a new owned descriptor.
        let fd = unsafe {
            libc::open(
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            )
        };
        let directory = owned(fd)?;
        let metadata = directory.metadata()?;
        Ok(Self {
            identity: RootIdentity {
                device: metadata.dev(),
                inode: metadata.ino(),
            },
            directory,
            path,
        })
    }
    pub fn directory(&self, path: &str) -> Result<File> {
        open_at(&self.directory, &relative_path(path)?, true)
    }
    pub fn list(&self, path: &str, limit: usize) -> Result<Vec<DirEntry>> {
        list_directory(&self.directory(path)?, limit)
    }
    pub fn read(&self, path: &str) -> Result<Vec<u8>> {
        let file = open_at(&self.directory, &relative_path(path)?, false)?;
        read_regular(file)
    }
    // Resolve the parent separately: preview must not turn a missing directory
    // into a valid proposed file creation. Only an absent final entry is optional.
    pub fn read_optional(&self, path: &str) -> Result<Option<Vec<u8>>> {
        let target = self.file_target(path)?;
        match open_at(&target.parent, &target.name, false) {
            Ok(file) => Ok(Some(read_regular(file)?)),
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
            {
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }

    fn file_target(&self, path: &str) -> Result<FileTarget> {
        let relative = relative_path(path)?;
        ensure!(relative != ".", "file operation requires a file path");
        let name = relative.rsplit('/').next().context("file name missing")?;
        let parent = relative
            .strip_suffix(name)
            .unwrap_or("")
            .trim_end_matches('/');
        let parent = open_at(
            &self.directory,
            if parent.is_empty() { "." } else { parent },
            true,
        )?;
        Ok(FileTarget {
            parent,
            name: name.to_owned(),
        })
    }

    pub fn write(&self, path: &str, content: &[u8]) -> Result<()> {
        ensure!(
            content.len() as u64 <= READ_LIMIT,
            "file exceeds 64 MiB write limit"
        );
        let target = self.file_target(path)?;
        match stat_child(&target.parent, &target.name) {
            Ok(entry) => ensure!(
                entry.kind == EntryKind::File,
                "write requires a regular file"
            ),
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) => {}
            Err(error) => return Err(error),
        }
        // Never truncate an existing inode: it could be a hardlink outside the
        // machine. Rename replaces the directory entry, even if it races with a
        // symlink substitution, and cannot follow that symlink to its target.
        let temporary = CString::new(format!(".loom-write-{}", uuid::Uuid::new_v4()))?;
        let destination = CString::new(target.name)?;
        // SAFETY: parent owns a live directory fd, names are NUL terminated;
        // O_EXCL ensures the newly owned fd cannot refer to a substituted link.
        let mut file = owned(unsafe {
            libc::openat(
                target.parent.as_raw_fd(),
                temporary.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_CLOEXEC | libc::O_NOFOLLOW,
                // mode_t is u16 on macOS; C varargs promote to int, so pass c_uint.
                0o600 as libc::c_uint,
            )
        })?;
        let result = (|| -> Result<()> {
            file.write_all(content)?;
            // SAFETY: both names and the pinned parent remain live for renameat.
            let result = unsafe {
                libc::renameat(
                    target.parent.as_raw_fd(),
                    temporary.as_ptr(),
                    target.parent.as_raw_fd(),
                    destination.as_ptr(),
                )
            };
            if result < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            Ok(())
        })();
        if result.is_err() {
            // SAFETY: remove only our freshly created temporary directory entry.
            unsafe {
                libc::unlinkat(target.parent.as_raw_fd(), temporary.as_ptr(), 0);
            }
        }
        result
    }

    pub fn stat(&self, path: &str) -> Result<DirEntry> {
        let relative = relative_path(path)?;
        let mut names = relative.rsplitn(2, '/');
        let name = names.next().unwrap_or(".");
        let parent = names.next().unwrap_or(".");
        let parent = open_at(&self.directory, parent, true)?;
        stat_child(&parent, name)
    }
    pub async fn walk(&self, path: &str, limits: WalkLimits) -> Result<Vec<DirEntry>> {
        ensure!(limits.max_depth <= 256, "walk max_depth exceeds 256");
        ensure!(
            limits.max_entries <= 1_000_000,
            "walk max_entries exceeds 1000000"
        );
        let root = Arc::new(self.directory(path)?);
        if limits.max_depth == 0 {
            return Ok(Vec::new());
        }
        struct Job {
            path: String,
            depth: u32,
        }
        struct DirectoryBatch {
            job: Job,
            entries: Vec<DirEntry>,
        }
        let mut jobs = VecDeque::from([Job {
            path: String::new(),
            depth: 1,
        }]);
        let mut running = tokio::task::JoinSet::new();
        let mut entries = Vec::new();
        let mut bytes = 0_usize;
        let result = async {
            while !jobs.is_empty() || !running.is_empty() {
                while running.len() < 64 {
                    let Some(job) = jobs.pop_front() else { break };
                    let root = root.clone();
                    running.spawn_blocking(move || -> Result<DirectoryBatch> {
                        let relative = if job.path.is_empty() { "." } else { &job.path };
                        let directory = open_at(&root, relative, true)
                            .with_context(|| format!("walk directory {relative}"))?;
                        let entries = list_directory(&directory, limits.max_entries)
                            .with_context(|| format!("walk directory {relative}"))?;
                        Ok(DirectoryBatch { job, entries })
                    });
                }
                let batch = running
                    .join_next()
                    .await
                    .context("walk worker missing")???;
                ensure!(
                    entries.len().saturating_add(batch.entries.len()) <= limits.max_entries,
                    "walk entry limit exceeded"
                );
                for mut entry in batch.entries {
                    if !batch.job.path.is_empty() {
                        entry.name = format!("{}/{}", batch.job.path, entry.name);
                    }
                    bytes = bytes.saturating_add(entry.name.len() + 32);
                    ensure!(bytes <= RESULT_LIMIT, "walk result exceeds 64 MiB");
                    if entry.kind == EntryKind::Directory && batch.job.depth < limits.max_depth {
                        jobs.push_back(Job {
                            path: entry.name.clone(),
                            depth: batch.job.depth + 1,
                        });
                    }
                    entries.push(entry);
                }
            }
            Ok::<(), anyhow::Error>(())
        }
        .await;
        // Blocking jobs cannot be canceled once started; finish bounded in-flight
        // reads before releasing this execution's filesystem resources.
        while running.join_next().await.is_some() {}
        result?;
        entries.sort_unstable_by(|left, right| left.name.cmp(&right.name));
        Ok(entries)
    }
}

pub(crate) fn relative_path(path: &str) -> Result<String> {
    ensure!(path.len() <= 4096, "machine path exceeds 4096 bytes");
    ensure!(!path.as_bytes().contains(&0), "machine path contains NUL");
    let mut parts = Vec::new();
    for part in path.split('/') {
        ensure!(part != "..", "parent traversal is forbidden");
        if !part.is_empty() && part != "." {
            parts.push(part);
        }
    }
    Ok(if parts.is_empty() {
        ".".into()
    } else {
        parts.join("/")
    })
}

pub(crate) fn open_at(parent: &File, path: &str, directory: bool) -> Result<File> {
    #[cfg(target_os = "linux")]
    {
        linux::open_at(parent, path, directory)
    }
    #[cfg(target_os = "macos")]
    {
        let mut current = None;
        let names: Vec<_> = path.split('/').collect();
        for (index, name) in names.iter().enumerate() {
            let name = CString::new(*name)?;
            let is_directory = directory || index + 1 != names.len();
            let flags = libc::O_RDONLY
                | libc::O_CLOEXEC
                | libc::O_NOFOLLOW
                | libc::O_NONBLOCK
                | if is_directory { libc::O_DIRECTORY } else { 0 };
            let fd = current
                .as_ref()
                .map_or(parent.as_raw_fd(), |file: &File| file.as_raw_fd());
            // SAFETY: fd remains owned by parent/current and name is NUL terminated.
            current = Some(owned(unsafe { libc::openat(fd, name.as_ptr(), flags) })?);
        }
        current.context("empty relative path")
    }
}

pub(crate) fn read_regular(file: File) -> Result<Vec<u8>> {
    let metadata = file.metadata()?;
    ensure!(metadata.is_file(), "read requires a regular file");
    ensure!(
        metadata.len() <= READ_LIMIT,
        "file exceeds 64 MiB read limit"
    );
    let mut bytes = Vec::new();
    file.take(READ_LIMIT + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= READ_LIMIT,
        "file exceeds 64 MiB read limit"
    );
    Ok(bytes)
}

pub(crate) fn list_directory(directory: &File, limit: usize) -> Result<Vec<DirEntry>> {
    #[cfg(target_os = "macos")]
    let mut entries = darwin::list(directory, limit)?;
    #[cfg(target_os = "linux")]
    let mut entries = linux::list(directory, limit)?;
    entries.sort_unstable_by(|a, b| a.name.cmp(&b.name));
    Ok(entries)
}

fn owned(fd: RawFd) -> Result<File> {
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    // SAFETY: successful open creates one descriptor, transferred exactly once.
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn kind(mode: libc::mode_t) -> EntryKind {
    match mode & libc::S_IFMT {
        libc::S_IFREG => EntryKind::File,
        libc::S_IFDIR => EntryKind::Directory,
        libc::S_IFLNK => EntryKind::Symlink,
        _ => EntryKind::Other,
    }
}

fn stat_child(parent: &File, name: &str) -> Result<DirEntry> {
    let c_name = CString::new(name)?;
    let mut metadata = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: metadata points to writable stat storage; no symlinks are followed.
    let result = unsafe {
        libc::fstatat(
            parent.as_raw_fd(),
            c_name.as_ptr(),
            metadata.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    // SAFETY: fstatat succeeded and initialized every field in stat.
    let metadata = unsafe { metadata.assume_init() };
    let kind = kind(metadata.st_mode);
    let size = if kind == EntryKind::File {
        u64::try_from(metadata.st_size).context("negative file length")?
    } else {
        0
    };
    Ok(DirEntry {
        name: name.into(),
        size,
        kind,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires LOOM_SCAN_FIXTURE and an otherwise idle host; run release mode"]
    async fn native_walk_timing() -> Result<()> {
        #[derive(Serialize)]
        struct Measurement {
            files: usize,
            directories: usize,
            winner: String,
            size: u64,
            samples_ms: Vec<f64>,
            median_ms: f64,
            limit_ms: f64,
        }
        let fixture = std::env::var_os("LOOM_SCAN_FIXTURE")
            .context("LOOM_SCAN_FIXTURE must name the standard 10000-file fixture")?;
        let root = PinnedRoot::open(Path::new(&fixture))?;
        let mut samples_ms = Vec::with_capacity(7);
        for _ in 0..7 {
            let start = std::time::Instant::now();
            let entries = root
                .walk(
                    ".",
                    WalkLimits {
                        max_depth: 64,
                        max_entries: 20_000,
                    },
                )
                .await?;
            samples_ms.push(start.elapsed().as_secs_f64() * 1000.0);
            let files = entries
                .iter()
                .filter(|entry| entry.kind == EntryKind::File)
                .count();
            let directories = entries
                .iter()
                .filter(|entry| entry.kind == EntryKind::Directory)
                .count();
            ensure!(
                files == 10_000 && directories == 256,
                "unexpected fixture counts: {files} files, {directories} directories"
            );
            let winner = entries
                .iter()
                .filter(|entry| entry.kind == EntryKind::File)
                .max_by(|left, right| {
                    left.size
                        .cmp(&right.size)
                        .then_with(|| right.name.cmp(&left.name))
                })
                .context("fixture contains no regular files")?;
            ensure!(
                winner.name == "nested/middle/deep/alpha.dat" && winner.size == 8193,
                "fixture tie winner changed"
            );
            ensure!(
                entries
                    .iter()
                    .any(|entry| entry.name == "nested/middle/deep/zeta.dat"
                        && entry.kind == EntryKind::File
                        && entry.size == 8193),
                "fixture tied competitor missing"
            );
        }
        let mut ordered = samples_ms.clone();
        ordered.sort_by(f64::total_cmp);
        let measurement = Measurement {
            files: 10_000,
            directories: 256,
            winner: "nested/middle/deep/alpha.dat".into(),
            size: 8193,
            samples_ms,
            median_ms: ordered[3],
            limit_ms: 8.0,
        };
        println!("{}", serde_json::to_string(&measurement)?);
        ensure!(
            measurement.median_ms < measurement.limit_ms,
            "native walk median {:.3} ms must be below 8 ms",
            measurement.median_ms
        );
        Ok(())
    }

    #[test]
    fn pinned_root_closes_canonicalize_then_rename_escape() -> Result<()> {
        let parent = tempfile::tempdir()?;
        let root_path = parent.path().join("root");
        let displaced = parent.path().join("displaced");
        let outside = parent.path().join("outside");
        std::fs::create_dir(&root_path)?;
        std::fs::create_dir(&outside)?;
        std::fs::write(root_path.join("file"), b"inside")?;
        std::fs::write(outside.join("file"), b"outside-secret")?;
        let root = PinnedRoot::open(&root_path)?;
        // The old check/use path authorizes this pathname before its parent is swapped.
        let previously_authorized = root_path.join("file").canonicalize()?;
        std::fs::rename(&root_path, &displaced)?;
        symlink(&outside, &root_path)?;
        assert_eq!(
            std::fs::read(previously_authorized)?,
            b"outside-secret",
            "old path-based control must expose the race"
        );
        assert_eq!(root.read("file")?, b"inside");
        assert_eq!(root.stat("file")?.size, 6);
        let entries = root.list("/", 10)?;
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "file");
        assert_eq!(entries[0].size, 6);
        Ok(())
    }

    #[test]
    fn nofollow_and_structural_parent_rules_apply_to_every_operation() -> Result<()> {
        let root_dir = tempfile::tempdir()?;
        let outside = tempfile::tempdir()?;
        std::fs::write(outside.path().join("secret"), b"secret")?;
        symlink(outside.path(), root_dir.path().join("link"))?;
        symlink(
            outside.path().join("secret"),
            root_dir.path().join("file-link"),
        )?;
        let root = PinnedRoot::open(root_dir.path())?;
        assert!(root.read("link/secret").is_err());
        assert!(root.read("file-link").is_err());
        assert!(root.list("link", 10).is_err());
        assert!(root.stat("link/secret").is_err());
        assert_eq!(root.stat("file-link")?.kind, EntryKind::Symlink);
        for path in ["..", "a/../b", "/../secret", "./../"] {
            assert!(root.read(path).is_err());
            assert!(root.list(path, 10).is_err());
            assert!(root.stat(path).is_err());
        }
        Ok(())
    }

    #[tokio::test]
    async fn walk_is_sorted_bounded_and_does_not_follow_links() -> Result<()> {
        let root_dir = tempfile::tempdir()?;
        std::fs::create_dir(root_dir.path().join("sub"))?;
        std::fs::write(root_dir.path().join("z"), b"abc")?;
        std::fs::write(root_dir.path().join("sub/a"), b"hello")?;
        symlink(root_dir.path(), root_dir.path().join("sub/cycle"))?;
        let root = PinnedRoot::open(root_dir.path())?;
        let entries = root
            .walk(
                "/",
                WalkLimits {
                    max_depth: 64,
                    max_entries: 10,
                },
            )
            .await?;
        let names: Vec<_> = entries.iter().map(|entry| entry.name.as_str()).collect();
        assert_eq!(names, ["sub", "sub/a", "sub/cycle", "z"]);
        assert_eq!(entries[1].size, 5);
        assert_eq!(entries[2].kind, EntryKind::Symlink);
        assert_eq!(
            root.walk(
                "/",
                WalkLimits {
                    max_depth: 1,
                    max_entries: 10
                }
            )
            .await?
            .len(),
            2
        );
        assert!(
            root.walk(
                "/",
                WalkLimits {
                    max_depth: 64,
                    max_entries: 3
                }
            )
            .await
            .is_err()
        );
        assert!(
            root.walk(
                "/",
                WalkLimits {
                    max_depth: 0,
                    max_entries: 0
                }
            )
            .await?
            .is_empty()
        );
        Ok(())
    }
}
