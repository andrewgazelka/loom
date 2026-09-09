use super::{DirEntry, EntryKind, File, Result, owned};
use anyhow::{Context, ensure};
use std::{ffi::CString, os::fd::AsRawFd};

pub(super) fn open_at(parent: &File, path: &str, directory: bool) -> Result<File> {
    #[repr(C)]
    struct OpenHow {
        flags: u64,
        mode: u64,
        resolve: u64,
    }
    let path = CString::new(path)?;
    let how = OpenHow {
        flags: (libc::O_RDONLY
            | libc::O_CLOEXEC
            | libc::O_NOFOLLOW
            | libc::O_NONBLOCK
            | if directory { libc::O_DIRECTORY } else { 0 }) as u64,
        mode: 0,
        resolve: libc::RESOLVE_BENEATH | libc::RESOLVE_NO_SYMLINKS,
    };
    // SAFETY: openat2 reads exactly the provided initialized OpenHow and CString.
    let fd = unsafe {
        libc::syscall(
            libc::SYS_openat2,
            parent.as_raw_fd(),
            path.as_ptr(),
            &how,
            std::mem::size_of::<OpenHow>(),
        )
    };
    owned(i32::try_from(fd).context("invalid openat2 descriptor")?)
}

struct NativeEntry {
    name: String,
    kind: u8,
}

fn records(bytes: &[u8]) -> Result<Vec<NativeEntry>> {
    let mut entries = Vec::new();
    let mut offset = 0;
    while offset < bytes.len() {
        let remaining = &bytes[offset..];
        ensure!(remaining.len() >= 20, "truncated getdents64 record");
        let length = u16::from_ne_bytes([remaining[16], remaining[17]]) as usize;
        ensure!(
            length >= 20 && length <= remaining.len(),
            "invalid getdents64 record length"
        );
        let names = &remaining[19..length];
        let end = names
            .iter()
            .position(|byte| *byte == 0)
            .context("unterminated getdents64 name")?;
        let name =
            std::str::from_utf8(&names[..end]).context("directory entry name is not UTF8")?;
        ensure!(
            !name.is_empty() && !name.contains('/'),
            "invalid getdents64 name"
        );
        if name != "." && name != ".." {
            entries.push(NativeEntry {
                name: name.into(),
                kind: remaining[18],
            });
        }
        offset += length;
    }
    Ok(entries)
}

pub(super) fn list(directory: &File, limit: usize) -> Result<Vec<DirEntry>> {
    let mut buffer = vec![0_u8; 64 * 1024];
    let mut entries = Vec::new();
    loop {
        // SAFETY: writable buffer is valid for its length; directory owns the fd.
        let count = unsafe {
            libc::syscall(
                libc::SYS_getdents64,
                directory.as_raw_fd(),
                buffer.as_mut_ptr(),
                buffer.len(),
            )
        };
        if count < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        if count == 0 {
            break;
        }
        let count = usize::try_from(count).context("invalid getdents64 count")?;
        ensure!(count <= buffer.len(), "getdents64 overflow");
        for entry in records(&buffer[..count])? {
            ensure!(entries.len() < limit, "directory entry limit exceeded");
            let kind = match entry.kind {
                libc::DT_DIR => EntryKind::Directory,
                libc::DT_LNK => EntryKind::Symlink,
                libc::DT_REG => EntryKind::File,
                libc::DT_UNKNOWN => EntryKind::Other,
                _ => EntryKind::Other,
            };
            if entry.kind == libc::DT_REG || entry.kind == libc::DT_UNKNOWN {
                let name = CString::new(entry.name.as_str())?;
                let mut metadata = std::mem::MaybeUninit::<libc::statx>::zeroed();
                // SAFETY: statx writes the initialized output; NOFOLLOW prevents
                // a rename to a symlink between enumeration and metadata lookup.
                let result = unsafe {
                    libc::statx(
                        directory.as_raw_fd(),
                        name.as_ptr(),
                        libc::AT_SYMLINK_NOFOLLOW | libc::AT_NO_AUTOMOUNT,
                        libc::STATX_TYPE | libc::STATX_SIZE,
                        metadata.as_mut_ptr(),
                    )
                };
                if result < 0 {
                    return Err(std::io::Error::last_os_error().into());
                }
                // SAFETY: zeroed statx is valid and the successful call populated it.
                let metadata = unsafe { metadata.assume_init() };
                ensure!(
                    metadata.stx_mask & libc::STATX_TYPE != 0,
                    "statx omitted file type"
                );
                let kind = super::kind(metadata.stx_mode as libc::mode_t);
                let size = if kind == EntryKind::File {
                    ensure!(
                        metadata.stx_mask & libc::STATX_SIZE != 0,
                        "statx omitted file size"
                    );
                    metadata.stx_size
                } else {
                    0
                };
                entries.push(DirEntry {
                    name: entry.name,
                    size,
                    kind,
                });
            } else {
                entries.push(DirEntry {
                    name: entry.name,
                    size: 0,
                    kind,
                });
            }
        }
    }
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn getdents_rejects_truncated_and_nonadvancing_records() {
        assert!(records(&[0; 19]).is_err());
        assert!(records(&[0; 24]).is_err());
        let mut record = [0_u8; 24];
        record[16..18].copy_from_slice(&24_u16.to_ne_bytes());
        record[18] = libc::DT_REG;
        record[19..21].copy_from_slice(b"a\0");
        assert_eq!(records(&record).unwrap()[0].name, "a");
        record[19..].fill(b'x');
        assert!(records(&record).is_err());
    }
}
