//! Machine filesystem authority is an open directory, never a re-resolved pathname.
use anyhow::{Context, Result, ensure};
use loom_proto::{DirEntry, EntryKind};
use serde::{Deserialize, Serialize};
use std::{
    collections::VecDeque,
    ffi::CString,
    fs::File,
    io::Read,
    os::fd::{AsRawFd, FromRawFd, RawFd},
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::{Condvar, Mutex},
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
    pub fn stat(&self, path: &str) -> Result<DirEntry> {
        let relative = relative_path(path)?;
        let mut names = relative.rsplitn(2, '/');
        let name = names.next().unwrap_or(".");
        let parent = names.next().unwrap_or(".");
        let parent = open_at(&self.directory, parent, true)?;
        stat_child(&parent, name)
    }
    pub fn walk(&self, path: &str, limits: WalkLimits) -> Result<Vec<DirEntry>> {
        ensure!(limits.max_depth <= 256, "walk max_depth exceeds 256");
        ensure!(
            limits.max_entries <= 1_000_000,
            "walk max_entries exceeds 1000000"
        );
        let root = self.directory(path)?;
        if limits.max_depth == 0 {
            return Ok(Vec::new());
        }
        struct Job {
            path: String,
            prefix: String,
            depth: u32,
        }
        struct State {
            jobs: VecDeque<Job>,
            active: usize,
            entries: Vec<DirEntry>,
            bytes: usize,
            error: Option<anyhow::Error>,
        }
        let state = Mutex::new(State {
            jobs: VecDeque::from([Job {
                path: ".".into(),
                prefix: String::new(),
                depth: 1,
            }]),
            active: 0,
            entries: Vec::new(),
            bytes: 0,
            error: None,
        });
        let changed = Condvar::new();
        let workers = std::thread::available_parallelism()
            .map(usize::from)
            .unwrap_or(1)
            .min(8);
        std::thread::scope(|scope| {
            for _ in 0..workers {
                let state = &state;
                let changed = &changed;
                let root = &root;
                scope.spawn(move || {
                    loop {
                        let job = {
                            let mut state = state.lock().unwrap();
                            loop {
                                if state.error.is_some() {
                                    return;
                                }
                                if let Some(job) = state.jobs.pop_front() {
                                    state.active += 1;
                                    break job;
                                }
                                if state.active == 0 {
                                    return;
                                }
                                state = changed.wait(state).unwrap();
                            }
                        };
                        let outcome = (|| -> Result<Vec<DirEntry>> {
                            list_directory(&open_at(root, &job.path, true)?, limits.max_entries)
                        })();
                        let mut state = state.lock().unwrap();
                        state.active -= 1;
                        match outcome {
                            Err(error) => {
                                state.error =
                                    Some(error.context(format!("walk directory {}", job.path)))
                            }
                            Ok(entries) => {
                                if state.entries.len().saturating_add(entries.len())
                                    > limits.max_entries
                                {
                                    state.error =
                                        Some(anyhow::anyhow!("walk entry limit exceeded"));
                                } else {
                                    for mut entry in entries {
                                        entry.name = if job.prefix.is_empty() {
                                            entry.name
                                        } else {
                                            format!("{}/{}", job.prefix, entry.name)
                                        };
                                        state.bytes =
                                            state.bytes.saturating_add(entry.name.len() + 32);
                                        if state.bytes > RESULT_LIMIT {
                                            state.error =
                                                Some(anyhow::anyhow!("walk result exceeds 64 MiB"));
                                            break;
                                        }
                                        if entry.kind == EntryKind::Directory
                                            && job.depth < limits.max_depth
                                        {
                                            state.jobs.push_back(Job {
                                                path: entry.name.clone(),
                                                prefix: entry.name.clone(),
                                                depth: job.depth + 1,
                                            });
                                        }
                                        state.entries.push(entry);
                                    }
                                }
                            }
                        }
                        changed.notify_all();
                    }
                });
            }
        });
        let mut state = state.into_inner().unwrap();
        if let Some(error) = state.error {
            return Err(error);
        }
        state.entries.sort_unstable_by(|a, b| a.name.cmp(&b.name));
        Ok(state.entries)
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

    #[test]
    fn walk_is_sorted_bounded_and_does_not_follow_links() -> Result<()> {
        let root_dir = tempfile::tempdir()?;
        std::fs::create_dir(root_dir.path().join("sub"))?;
        std::fs::write(root_dir.path().join("z"), b"abc")?;
        std::fs::write(root_dir.path().join("sub/a"), b"hello")?;
        symlink(root_dir.path(), root_dir.path().join("sub/cycle"))?;
        let root = PinnedRoot::open(root_dir.path())?;
        let entries = root.walk(
            "/",
            WalkLimits {
                max_depth: 64,
                max_entries: 10,
            },
        )?;
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
            )?
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
            .is_err()
        );
        assert!(
            root.walk(
                "/",
                WalkLimits {
                    max_depth: 0,
                    max_entries: 0
                }
            )?
            .is_empty()
        );
        Ok(())
    }
}
