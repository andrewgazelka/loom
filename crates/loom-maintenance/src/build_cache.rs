use anyhow::{Result, ensure};
use loom_build::Builder;
use serde::Serialize;
use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

pub struct CachePolicy {
    pub max_bytes: u64,
    pub max_age: Duration,
    pub max_entries: usize,
}

#[derive(Debug, Serialize)]
pub struct CacheEviction {
    pub removed_entries: usize,
    pub removed_bytes: u64,
    pub remaining_bytes: u64,
    pub byte_limit_met: bool,
}

struct Entry {
    path: PathBuf,
    bytes: u64,
    modified: SystemTime,
}

/// Evict rebuildable filesystem artifacts only. CAS is never touched. The
/// builder gate excludes active builds; callers must share their daemon Builder.
pub async fn evict_build_cache(builder: &Builder, policy: &CachePolicy) -> Result<CacheEviction> {
    ensure!(
        (1..=1000).contains(&policy.max_entries),
        "eviction limit must be 1..=1000"
    );
    builder
        .with_cache_exclusive(|root| evict(root, policy))
        .await?
}

fn measure(path: &Path) -> Result<Entry> {
    let metadata = fs::symlink_metadata(path)?;
    let mut entry = Entry {
        path: path.to_owned(),
        bytes: 0,
        modified: metadata.modified()?,
    };
    if metadata.is_symlink() {
        return Ok(entry);
    }
    if metadata.is_file() {
        entry.bytes = metadata.len();
    } else if metadata.is_dir() {
        for child in fs::read_dir(path)? {
            let child = measure(&child?.path())?;
            entry.bytes = entry
                .bytes
                .checked_add(child.bytes)
                .ok_or_else(|| anyhow::anyhow!("cache size overflow"))?;
            entry.modified = entry.modified.max(child.modified);
        }
    }
    Ok(entry)
}

fn evict(root: &Path, policy: &CachePolicy) -> Result<CacheEviction> {
    let mut entries = Vec::new();
    let mut remaining_bytes = 0_u64;
    if root.exists() {
        ensure!(
            !fs::symlink_metadata(root)?.is_symlink(),
            "cache root must not be a symlink"
        );
        for child in fs::read_dir(root)? {
            let child = child?;
            let name = child.file_name();
            let name = name.to_string_lossy();
            // These are the only directory names owned by Builder.
            if name != "rust-target"
                && !(name.len() == 64 && name.bytes().all(|byte| byte.is_ascii_hexdigit()))
            {
                continue;
            }
            if !child.file_type()?.is_dir() {
                continue;
            }
            let entry = measure(&child.path())?;
            remaining_bytes = remaining_bytes
                .checked_add(entry.bytes)
                .ok_or_else(|| anyhow::anyhow!("cache size overflow"))?;
            entries.push(entry);
        }
    }
    entries.sort_by_key(|entry| entry.modified);
    let now = SystemTime::now();
    let mut removed_entries = 0;
    let mut removed_bytes = 0;
    for entry in entries {
        if removed_entries == policy.max_entries {
            break;
        }
        let expired = now.duration_since(entry.modified).unwrap_or_default() >= policy.max_age;
        if remaining_bytes <= policy.max_bytes && !expired {
            continue;
        }
        fs::remove_dir_all(&entry.path)?;
        removed_entries += 1;
        removed_bytes += entry.bytes;
        remaining_bytes -= entry.bytes;
    }
    Ok(CacheEviction {
        removed_entries,
        removed_bytes,
        remaining_bytes,
        byte_limit_met: remaining_bytes <= policy.max_bytes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounds_eviction_and_preserves_unowned_paths() -> Result<()> {
        let root = tempfile::tempdir()?;
        for name in ["a".repeat(64), "b".repeat(64), "unowned".into()] {
            fs::create_dir(root.path().join(&name))?;
            fs::write(root.path().join(name).join("component.wasm"), [0_u8; 10])?;
        }
        let policy = CachePolicy {
            max_bytes: 0,
            max_age: Duration::MAX,
            max_entries: 1,
        };
        let result = evict(root.path(), &policy)?;
        assert_eq!(result.removed_entries, 1);
        assert_eq!(result.remaining_bytes, 10);
        assert!(!result.byte_limit_met);
        assert!(root.path().join("unowned/component.wasm").exists());
        Ok(())
    }

    #[tokio::test]
    async fn maintenance_waits_for_builder_gate() -> Result<()> {
        let root = tempfile::tempdir()?;
        let builder = Builder::new(root.path().to_owned(), loom_store::Store::memory()?);
        let policy = CachePolicy {
            max_bytes: u64::MAX,
            max_age: Duration::MAX,
            max_entries: 1,
        };
        builder
            .with_cache_exclusive(|_| {
                let mut eviction = Box::pin(evict_build_cache(&builder, &policy));
                let waker = std::task::Waker::noop();
                let mut context = std::task::Context::from_waker(waker);
                assert!(std::future::Future::poll(eviction.as_mut(), &mut context).is_pending());
            })
            .await?;
        assert!(evict_build_cache(&builder, &policy).await?.byte_limit_met);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn expiration_removes_cache_without_following_symlinks() -> Result<()> {
        let root = tempfile::tempdir()?;
        let outside = tempfile::tempdir()?;
        fs::write(outside.path().join("keep"), b"user data")?;
        let directory = root.path().join("a".repeat(64));
        fs::create_dir(&directory)?;
        std::os::unix::fs::symlink(outside.path(), directory.join("external"))?;
        fs::write(directory.join("component.wasm"), [0_u8; 10])?;
        let policy = CachePolicy {
            max_bytes: u64::MAX,
            max_age: Duration::ZERO,
            max_entries: 1,
        };
        let result = evict(root.path(), &policy)?;
        assert_eq!(result.removed_bytes, 10);
        assert!(!directory.exists());
        assert_eq!(fs::read(outside.path().join("keep"))?, b"user data");
        Ok(())
    }
}
