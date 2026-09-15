//! Copy an immutable image into the driver's private, size-bounded tmpfs.
use anyhow::{Context, Result, bail, ensure};
use std::{
    collections::VecDeque,
    ffi::OsString,
    fs::{self, OpenOptions, Permissions},
    os::unix::fs::{OpenOptionsExt, PermissionsExt, symlink},
    path::{Component, Path, PathBuf},
};

struct DirectoryMode {
    path: PathBuf,
    permissions: Permissions,
}
struct CopyEntry {
    source: PathBuf,
    destination: PathBuf,
}
pub fn prepare(source: &str, destination: &str) -> Result<PathBuf> {
    let source = Path::new(source);
    let destination = Path::new(destination);
    ensure!(
        source.is_absolute() && destination.is_absolute(),
        "image and guest roots must be absolute"
    );
    ensure!(
        !fs::symlink_metadata(source)?.file_type().is_symlink()
            && !fs::symlink_metadata(destination)?.file_type().is_symlink(),
        "image and guest roots must not be symlinks"
    );
    let source = source.canonicalize()?;
    let destination = destination.canonicalize()?;
    ensure!(
        source.is_dir() && destination.is_dir(),
        "image and guest roots must be directories"
    );
    ensure!(
        !source.starts_with(&destination) && !destination.starts_with(&source),
        "image and guest roots must be separate trees"
    );
    ensure!(
        fs::read_dir(&destination)?.next().is_none(),
        "guest root must be empty before boot"
    );
    // Source is immutable and read-only in the driver-created mount namespace;
    // destination has no guest or concurrent writers until this copy completes.
    let mut pending = vec![CopyEntry {
        source,
        destination: destination.clone(),
    }];
    let mut directory_modes = Vec::new();
    while let Some(entry) = pending.pop() {
        let metadata = fs::symlink_metadata(&entry.source)?;
        if metadata.is_dir() {
            if entry.destination != destination {
                fs::create_dir(&entry.destination)?;
            }
            directory_modes.push(DirectoryMode {
                path: entry.destination.clone(),
                permissions: metadata.permissions(),
            });
            for child in fs::read_dir(&entry.source)? {
                let child = child?;
                pending.push(CopyEntry {
                    source: child.path(),
                    destination: entry.destination.join(child.file_name()),
                });
            }
        } else if metadata.file_type().is_symlink() {
            // Preserve a guest link's literal target; never follow it on the host.
            symlink(fs::read_link(&entry.source)?, &entry.destination)?;
        } else if metadata.is_file() {
            let mut input = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&entry.source)?;
            let mut output = OpenOptions::new()
                .write(true)
                .create_new(true)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&entry.destination)?;
            std::io::copy(&mut input, &mut output)
                .with_context(|| format!("copy image file {}", entry.source.display()))?;
            output.set_permissions(metadata.permissions())?;
        } else {
            bail!("unsupported image node: {}", entry.source.display());
        }
    }
    // Defer read-only directory permissions until children have been populated.
    for directory in directory_modes.into_iter().rev() {
        fs::set_permissions(directory.path, directory.permissions)?;
    }
    Ok(destination)
}

/// Resolve guest symlinks with guest-root semantics, including absolute links.
/// Host canonicalize would incorrectly resolve `/bin/sh` links in the host tree.
fn guest_path(root: &Path, path: &str) -> Result<PathBuf> {
    ensure!(path.starts_with('/'), "guest path must be absolute");
    let mut pending = components(Path::new(path))?;
    let mut relative = PathBuf::new();
    let mut links = 0;
    while let Some(part) = pending.pop_front() {
        if part == "/" {
            relative.clear();
            continue;
        }
        if part == ".." {
            relative.pop();
            continue;
        }
        if part == "." {
            continue;
        }
        let candidate = root.join(&relative).join(&part);
        let metadata = fs::symlink_metadata(&candidate)
            .with_context(|| format!("guest path {path}: {}", candidate.display()))?;
        if metadata.file_type().is_symlink() {
            links += 1;
            ensure!(links <= 40, "too many guest symlinks resolving {path}");
            let mut target = components(&fs::read_link(&candidate)?)?;
            target.append(&mut pending);
            pending = target;
        } else {
            relative.push(part);
        }
    }
    Ok(root.join(relative))
}
fn components(path: &Path) -> Result<VecDeque<OsString>> {
    path.components()
        .map(|part| {
            Ok(match part {
                Component::RootDir => OsString::from("/"),
                Component::CurDir => OsString::from("."),
                Component::ParentDir => OsString::from(".."),
                Component::Normal(value) => value.to_owned(),
                Component::Prefix(_) => bail!("unsupported guest path prefix"),
            })
        })
        .collect()
}
pub fn validate_command(root: &Path, command: &str, cwd: &str) -> Result<()> {
    let command = guest_path(root, command)?;
    let metadata = fs::metadata(&command)?;
    ensure!(
        metadata.is_file() && metadata.permissions().mode() & 0o111 != 0,
        "guest command is not an executable regular file: {}",
        command.display()
    );
    ensure!(
        guest_path(root, cwd)?.is_dir(),
        "guest cwd is not a directory"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn copies_modes_and_guest_links_without_following_host_targets() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("image");
        let destination = temp.path().join("guest");
        fs::create_dir_all(source.join("bin")).unwrap();
        fs::create_dir(&destination).unwrap();
        fs::write(source.join("bin/tool"), b"guest").unwrap();
        fs::set_permissions(source.join("bin/tool"), Permissions::from_mode(0o751)).unwrap();
        symlink("/bin/tool", source.join("bin/sh")).unwrap();
        symlink("/missing-host-secret", source.join("secret")).unwrap();
        prepare(source.to_str().unwrap(), destination.to_str().unwrap()).unwrap();
        validate_command(&destination, "/bin/sh", "/").unwrap();
        assert_eq!(
            fs::read_link(destination.join("secret")).unwrap(),
            Path::new("/missing-host-secret")
        );
        assert_eq!(
            fs::metadata(destination.join("bin/tool"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o751
        );
        assert!(validate_command(&destination, "/secret", "/").is_err());
        fs::write(destination.join("bin/tool"), b"changed").unwrap();
        assert_eq!(fs::read(source.join("bin/tool")).unwrap(), b"guest");
    }
    #[test]
    fn rejects_cycles_overlap_nonempty_roots_and_nonexecutables() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        symlink("cycle", root.join("cycle")).unwrap();
        fs::write(root.join("plain"), b"x").unwrap();
        assert!(validate_command(root, "/cycle", "/").is_err());
        assert!(validate_command(root, "/plain", "/").is_err());
        assert!(prepare(root.to_str().unwrap(), root.to_str().unwrap()).is_err());
        let source = tempfile::tempdir().unwrap();
        assert!(prepare(source.path().to_str().unwrap(), root.to_str().unwrap()).is_err());
    }
}
