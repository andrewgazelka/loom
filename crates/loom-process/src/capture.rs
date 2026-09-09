//! Bounded content observations of explicitly selected regular files. These are
//! before/after reads, not a syscall audit: metadata-only mutations are excluded,
//! and concurrent writers can contribute changes during the process interval.
use anyhow::{Context, Result, ensure};
use loom_store::Store;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    io::Read,
    path::{Component, Path, PathBuf},
};

const MAX_PATHS: usize = 64;
const MAX_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileChange {
    pub path: String,
    pub machine: String,
    pub before: Option<String>,
    pub after: Option<String>,
    pub captured: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnavailablePath {
    pub path: String,
    pub reason: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FilesystemCapture {
    pub scope: String,
    pub paths: Vec<String>,
    pub unavailable_reason: Option<String>,
    pub unavailable_paths: Vec<UnavailablePath>,
}
impl Default for FilesystemCapture {
    fn default() -> Self {
        Self {
            scope: "explicit_paths".into(),
            paths: Vec::new(),
            unavailable_reason: Some(
                "No filesystem paths were selected for observation; filesystem changes are unknown"
                    .into(),
            ),
            unavailable_paths: Vec::new(),
        }
    }
}
pub(crate) struct Capture {
    root: Result<std::fs::File, String>,
    machine: String,
    before: Vec<Before>,
    pub report: FilesystemCapture,
    pending: bool,
}
struct Before {
    path: PathBuf,
    cid: Option<String>,
}
pub(crate) struct Captured {
    pub changes: Vec<FileChange>,
    pub report: FilesystemCapture,
}
impl Capture {
    pub fn begin(store: &Store, machine: String, root: &Path, paths: &[PathBuf]) -> Self {
        let mut capture = Self {
            root: open_root(root).map_err(|error| error.to_string()),
            machine,
            before: Vec::new(),
            report: FilesystemCapture::default(),
            pending: false,
        };
        if paths.is_empty() {
            return capture;
        }
        capture.report.unavailable_reason =
            Some("Process has not finished; before/after changes are not yet available".into());
        if paths.len() > MAX_PATHS {
            capture.report.unavailable_reason = Some(format!(
                "Capture unavailable: at most {MAX_PATHS} explicit paths are allowed"
            ));
            return capture;
        }
        capture.pending = true;
        let mut seen = BTreeSet::new();
        for requested in paths {
            let outcome = relative_path(root, requested).and_then(|path| {
                let root = capture
                    .root
                    .as_ref()
                    .map_err(|error| anyhow::anyhow!(error.clone()))?;
                let cid = snapshot(store, root, &path)?;
                Ok(Before { path, cid })
            });
            match outcome {
                Ok(before) if seen.insert(before.path.clone()) => {
                    capture
                        .report
                        .paths
                        .push(before.path.to_string_lossy().into_owned());
                    capture.before.push(before);
                }
                Ok(_) => {}
                Err(error) => capture.report.unavailable_paths.push(UnavailablePath {
                    path: requested.to_string_lossy().into_owned(),
                    reason: format!("Before capture: {error}"),
                }),
            }
        }
        capture
    }
    pub fn finish(mut self, store: &Store) -> Captured {
        let mut changes = Vec::new();
        if self.pending {
            self.report.unavailable_reason = None;
        }
        for before in self.before {
            let outcome = self
                .root
                .as_ref()
                .map_err(|error| anyhow::anyhow!(error.clone()))
                .and_then(|root| snapshot(store, root, &before.path));
            match outcome {
                Ok(after) if before.cid != after => changes.push(FileChange {
                    path: before.path.to_string_lossy().into_owned(),
                    machine: self.machine.clone(),
                    before: before.cid,
                    after,
                    captured: true,
                }),
                Ok(_) => {}
                Err(error) => self.report.unavailable_paths.push(UnavailablePath {
                    path: before.path.to_string_lossy().into_owned(),
                    reason: format!("After capture: {error}"),
                }),
            }
        }
        Captured {
            changes,
            report: self.report,
        }
    }
}
fn relative_path(root: &Path, requested: &Path) -> Result<PathBuf> {
    let relative = if requested.is_absolute() {
        requested
            .strip_prefix(root)
            .context("capture path escapes machine root")?
    } else {
        requested
    };
    let mut path = PathBuf::new();
    for component in relative.components() {
        match component {
            Component::Normal(name) => path.push(name),
            Component::CurDir => {}
            _ => anyhow::bail!("capture path must stay within machine root"),
        }
    }
    ensure!(
        !path.as_os_str().is_empty(),
        "capture requires an explicit regular file"
    );
    Ok(path)
}
#[cfg(unix)]
fn open_root(root: &Path) -> Result<std::fs::File> {
    use rustix::fs::{Mode, OFlags};
    Ok(rustix::fs::open(
        root,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?
    .into())
}
#[cfg(not(unix))]
fn open_root(_: &Path) -> Result<std::fs::File> {
    anyhow::bail!("safe filesystem capture is unavailable on this platform")
}
#[cfg(unix)]
fn open_file(root: &std::fs::File, path: &Path) -> Result<Option<std::fs::File>> {
    use rustix::fs::{Mode, OFlags};
    let mut directory = root.try_clone()?;
    let mut components = path.components().peekable();
    while let Some(component) = components.next() {
        let final_component = components.peek().is_none();
        let mut flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK;
        if !final_component {
            flags |= OFlags::DIRECTORY;
        }
        match rustix::fs::openat(&directory, component.as_os_str(), flags, Mode::empty()) {
            Ok(fd) if final_component => return Ok(Some(fd.into())),
            Ok(fd) => directory = fd.into(),
            Err(rustix::io::Errno::NOENT) => return Ok(None),
            Err(error) => return Err(error.into()),
        }
    }
    anyhow::bail!("capture requires an explicit regular file")
}
#[cfg(not(unix))]
fn open_file(_: &std::fs::File, _: &Path) -> Result<Option<std::fs::File>> {
    anyhow::bail!("safe filesystem capture is unavailable on this platform")
}
fn snapshot(store: &Store, root: &std::fs::File, path: &Path) -> Result<Option<String>> {
    let Some(file) = open_file(root, path)? else {
        return Ok(None);
    };
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file(),
        "capture supports regular files only (no directories, devices, or symlinks)"
    );
    ensure!(
        metadata.len() <= MAX_BYTES,
        "file exceeds capture limit of {MAX_BYTES} bytes"
    );
    let mut bytes = Vec::new();
    file.take(MAX_BYTES + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= MAX_BYTES,
        "file exceeds capture limit of {MAX_BYTES} bytes"
    );
    let hash = store.put("blob", &bytes)?;
    Ok(Some(
        loom_proto::cid_for_hash(&hash, loom_proto::RAW_CODEC).map_err(anyhow::Error::msg)?,
    ))
}
