use crate::{Input, key::relative};
use anyhow::{Context, Result, ensure};
use loom_store::Store;
use std::{collections::BTreeMap, path::Path};

/// Put every regular file under `directory` in the store and describe it as action inputs,
/// keyed by its path relative to `directory`. Symlinks and special files are refused: an
/// input tree is bytes, not pointers to somewhere else on the machine.
pub fn ingest_directory(store: &Store, directory: &Path) -> Result<BTreeMap<String, Input>> {
    fn walk(
        store: &Store,
        root: &Path,
        directory: &Path,
        into: &mut BTreeMap<String, Input>,
    ) -> Result<()> {
        for entry in std::fs::read_dir(directory)? {
            let entry = entry?;
            let path = entry.path();
            let kind = entry.file_type()?;
            if kind.is_dir() {
                walk(store, root, &path, into)?;
            } else if kind.is_file() {
                let name = path
                    .strip_prefix(root)?
                    .to_str()
                    .context("input path is not UTF-8")?;
                // A backslash would fold into `/` on some platforms, making `a\b` collide with
                // `a/b`, so the name is refused rather than mapped.
                ensure!(
                    !name.contains('\\'),
                    "input path {name:?} contains a backslash"
                );
                let name = name.to_owned();
                let hash = store.put_file("blob", &path)?;
                into.insert(
                    name,
                    Input {
                        hash,
                        executable: is_executable(&path)?,
                    },
                );
            } else {
                anyhow::bail!(
                    "input tree entry {} is not a regular file or directory",
                    path.display()
                );
            }
        }
        Ok(())
    }
    let mut inputs = BTreeMap::new();
    walk(store, directory, directory, &mut inputs)?;
    Ok(inputs)
}

fn is_executable(path: &Path) -> Result<bool> {
    use std::os::unix::fs::PermissionsExt;
    Ok(std::fs::metadata(path)?.permissions().mode() & 0o111 != 0)
}

/// Lay the declared inputs out under `root`. Every file is an independent copy (cloned on APFS)
/// the tool may write to; executables are written fresh with mode 0755.
pub(crate) fn place_inputs(
    store: &Store,
    root: &Path,
    inputs: &BTreeMap<String, Input>,
) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    for (name, input) in inputs {
        let destination = root.join(relative(name)?);
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if input.executable {
            let bytes = store
                .get(&input.hash)?
                .with_context(|| format!("input {name:?} ({}) is not in the store", input.hash))?;
            std::fs::write(&destination, bytes)?;
            std::fs::set_permissions(&destination, std::fs::Permissions::from_mode(0o755))?;
        } else {
            ensure!(
                store.has_object(&input.hash)?,
                "input {name:?} ({}) is not in the store",
                input.hash
            );
            store.restore_to(&input.hash, &destination)?;
        }
    }
    Ok(())
}
