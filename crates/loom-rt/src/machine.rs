use crate::{Runtime, required_str};
use anyhow::{Context, Result, bail, ensure};
use loom_proto::{Actor, Lang, Value};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::path::{Path, PathBuf};

#[derive(Serialize, Deserialize)]
struct Tree {
    entries: Vec<TreeEntry>,
}
#[derive(Serialize, Deserialize)]
struct TreeEntry {
    name: String,
    hash: String,
    directory: bool,
    executable: bool,
}

impl Runtime {
    pub fn create_machine(&self, root: &Path) -> Result<Actor> {
        let root = root.canonicalize()?;
        ensure!(root.is_dir(), "machine root must be a directory");
        let actor = Actor {
            id: uuid::Uuid::new_v4().to_string(),
            behavior_hash: "loom:machine".into(),
            lang: Lang::Rust,
            component_hash: None,
            last_seq: 0,
            created_seq: 0,
            parent: None,
        };
        self.inner
            .store
            .create_initialized_actor(&actor, &json!({"root":root}))
    }
    pub fn machine_root(&self, id: &str) -> Result<PathBuf> {
        let actor = self
            .inner
            .store
            .actor(id)?
            .context("machine actor not found")?;
        ensure!(
            actor.behavior_hash == "loom:machine",
            "actor is not a machine"
        );
        let state = self
            .inner
            .store
            .latest_snapshot(id, "loom:machine")?
            .context("machine state missing")?
            .state;
        Ok(PathBuf::from(required_str(&state, "root")?))
    }
    pub async fn start_process(&self, mut args: Value) -> Result<loom_process::ProcessState> {
        let machine = required_str(&args, "machine")?.to_owned();
        let root = self.machine_root(&machine)?;
        ensure!(
            root == Path::new("/"),
            "host processes require an explicitly unrestricted root machine; use hermetic exec for a restricted tree"
        );
        if args.get("path").is_none() {
            args["path"] = json!("/");
        }
        let cwd = self.machine_path(&args)?;
        let mut env = std::collections::BTreeMap::new();
        env.insert(
            "PATH".into(),
            std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into()),
        );
        if let Some(values) = args.get("env").and_then(Value::as_object) {
            for (name, value) in values {
                env.insert(
                    name.clone(),
                    value
                        .as_str()
                        .context("environment values must be strings")?
                        .to_owned(),
                );
            }
        }
        self.inner
            .processes
            .start(loom_process::ProcessSpec {
                machine,
                root,
                cwd,
                env,
                program: required_str(&args, "program")?.into(),
                args: args
                    .get("args")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default()
                    .into_iter()
                    .map(|arg| {
                        arg.as_str()
                            .map(str::to_owned)
                            .context("process arguments must be strings")
                    })
                    .collect::<Result<Vec<_>>>()?,
            })
            .await
    }
    pub(crate) fn machine_path(&self, args: &Value) -> Result<PathBuf> {
        let id = required_str(args, "machine")?;
        let actor = self
            .inner
            .store
            .actor(id)?
            .context("machine actor not found")?;
        ensure!(
            actor.behavior_hash == "loom:machine",
            "actor is not a machine"
        );
        let state = self
            .inner
            .store
            .latest_snapshot(id, "loom:machine")?
            .context("machine state missing")?
            .state;
        let root = PathBuf::from(required_str(&state, "root")?);
        let requested = required_str(args, "path")?;
        let path = root
            .join(requested.trim_start_matches('/'))
            .canonicalize()?;
        ensure!(path.starts_with(&root), "path escapes machine root");
        Ok(path)
    }
    pub(crate) async fn read_machine_file(&self, args: &Value) -> Result<Value> {
        use tokio::io::AsyncReadExt;
        const LIMIT: u64 = 64 * 1024 * 1024;
        let file = tokio::fs::File::open(self.machine_path(args)?).await?;
        ensure!(
            file.metadata().await?.len() <= LIMIT,
            "file exceeds 64 MiB read limit"
        );
        let mut bytes = Vec::new();
        file.take(LIMIT + 1).read_to_end(&mut bytes).await?;
        ensure!(
            bytes.len() as u64 <= LIMIT,
            "file exceeds 64 MiB read limit"
        );
        Ok(json!(String::from_utf8(bytes).context(
            "file is not UTF8; use snapshot for binary data"
        )?))
    }
    pub(crate) async fn snapshot_tree(&self, args: &Value) -> Result<Value> {
        let path = self.machine_path(args)?;
        let store = self.inner.store.clone();
        let hash = tokio::task::spawn_blocking(move || snapshot(&store, &path, &mut 0)).await??;
        Ok(json!({"$ref":hash}))
    }
    pub(crate) async fn hermetic_exec(&self, args: &Value) -> Result<Value> {
        ensure!(
            cfg!(target_os = "linux"),
            "hermetic execution requires Linux bubblewrap"
        );
        let hash = required_str(args, "tree")?.to_owned();
        let temp = tempfile::tempdir()?;
        let store = self.inner.store.clone();
        let root = temp.path().to_owned();
        tokio::task::spawn_blocking(move || materialize(&store, &hash, &root, 0)).await??;
        for mount in ["proc", "dev", "tmp"] {
            std::fs::create_dir_all(temp.path().join(mount))?;
        }
        let mut command = tokio::process::Command::new("bwrap");
        command
            .args([
                "--unshare-all",
                "--die-with-parent",
                "--new-session",
                "--ro-bind",
            ])
            .arg(temp.path())
            .arg("/")
            .args([
                "--proc",
                "/proc",
                "--dev",
                "/dev",
                "--tmpfs",
                "/tmp",
                "--clearenv",
                "--setenv",
                "PATH",
                "/bin:/usr/bin",
                "--chdir",
                "/",
                "--",
            ])
            .arg(required_str(args, "program")?);
        if let Some(arguments) = args.get("args").and_then(Value::as_array) {
            for argument in arguments {
                command.arg(
                    argument
                        .as_str()
                        .context("exec arguments must be strings")?,
                );
            }
        }
        self.execute_command(command).await
    }
}
fn snapshot(store: &loom_store::Store, path: &Path, count: &mut usize) -> Result<String> {
    let mut paths = std::fs::read_dir(path)?.collect::<std::io::Result<Vec<_>>>()?;
    paths.sort_by_key(|entry| entry.file_name());
    let mut entries = Vec::new();
    for entry in paths {
        *count += 1;
        ensure!(*count <= 100_000, "snapshot entry limit exceeded");
        let metadata = entry.path().symlink_metadata()?;
        ensure!(
            !metadata.file_type().is_symlink(),
            "snapshot symlinks are not supported"
        );
        let directory = metadata.is_dir();
        let hash = if directory {
            snapshot(store, &entry.path(), count)?
        } else {
            ensure!(metadata.is_file(), "snapshot requires regular files");
            ensure!(
                metadata.len() <= 64 * 1024 * 1024,
                "snapshot file exceeds 64 MiB"
            );
            store.put("blob", &std::fs::read(entry.path())?)?
        };
        #[cfg(unix)]
        let executable = {
            use std::os::unix::fs::PermissionsExt;
            metadata.permissions().mode() & 0o111 != 0
        };
        #[cfg(not(unix))]
        let executable = false;
        entries.push(TreeEntry {
            name: entry
                .file_name()
                .into_string()
                .map_err(|_| anyhow::anyhow!("non-UTF8 tree entry"))?,
            hash,
            directory,
            executable,
        });
    }
    store.put("tree", &serde_json::to_vec(&Tree { entries })?)
}
fn materialize(store: &loom_store::Store, hash: &str, path: &Path, depth: usize) -> Result<()> {
    ensure!(depth < 128, "tree depth limit exceeded");
    let tree: Tree = serde_json::from_slice(&store.get(hash)?.context("tree missing")?)?;
    for entry in tree.entries {
        if entry.name.is_empty()
            || entry.name == "."
            || entry.name == ".."
            || entry.name.contains('/')
            || entry.name.contains('\\')
        {
            bail!("invalid tree entry name");
        }
        let target = path.join(&entry.name);
        if entry.directory {
            std::fs::create_dir(&target)?;
            materialize(store, &entry.hash, &target, depth + 1)?;
        } else {
            std::fs::write(
                &target,
                store.get(&entry.hash)?.context("tree blob missing")?,
            )?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(
                    &target,
                    std::fs::Permissions::from_mode(if entry.executable { 0o755 } else { 0o644 }),
                )?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn snapshots_are_content_keyed_and_observations_are_scoped() -> Result<()> {
        let root = tempfile::tempdir()?;
        std::fs::write(root.path().join("hello"), "first")?;
        let runtime = Runtime::new(loom_store::Store::memory()?)?;
        let machine = runtime.create_machine(root.path())?;
        let descriptor = json!({"op":"fs.read","args":{"machine":machine.id,"path":"hello"}});
        assert_eq!(
            runtime.perform(descriptor.clone(), "one", 0).await?,
            json!("first")
        );
        let tree = runtime
            .snapshot_tree(&json!({"machine":machine.id,"path":"/"}))
            .await?;
        assert_eq!(
            tree,
            runtime
                .snapshot_tree(&json!({"machine":machine.id,"path":"/"}))
                .await?
        );
        std::fs::write(root.path().join("hello"), "second")?;
        assert_eq!(
            runtime.perform(descriptor.clone(), "one", 0).await?,
            json!("first")
        );
        assert_eq!(
            runtime.perform(descriptor, "two", 0).await?,
            json!("second")
        );
        assert_ne!(
            tree,
            runtime
                .snapshot_tree(&json!({"machine":machine.id,"path":"/"}))
                .await?
        );
        Ok(())
    }
    #[tokio::test]
    async fn oversized_read_is_rejected_before_loading() -> Result<()> {
        let root = tempfile::tempdir()?;
        std::fs::File::create(root.path().join("large"))?.set_len(64 * 1024 * 1024 + 1)?;
        let runtime = Runtime::new(loom_store::Store::memory()?)?;
        let machine = runtime.create_machine(root.path())?;
        let error = runtime
            .read_machine_file(&json!({"machine":machine.id,"path":"large"}))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("64 MiB"));
        Ok(())
    }
    #[cfg(target_os = "linux")]
    #[tokio::test]
    #[ignore = "requires Linux bubblewrap and LOOM_STATIC_BUSYBOX; run in the native Linux integration lane"]
    async fn hermetic_exec_uses_snapshot_and_caches_across_actors() -> Result<()> {
        let executable = std::env::var("LOOM_STATIC_BUSYBOX")
            .context("LOOM_STATIC_BUSYBOX must name a static busybox binary")?;
        let root = tempfile::tempdir()?;
        std::fs::create_dir(root.path().join("bin"))?;
        std::fs::copy(executable, root.path().join("bin/busybox"))?;
        std::fs::write(root.path().join("input"), "snapshot-data")?;
        let runtime = Runtime::new(loom_store::Store::memory()?)?;
        let machine = runtime.create_machine(root.path())?;
        let tree = runtime
            .snapshot_tree(&json!({"machine":machine.id,"path":"/"}))
            .await?;
        std::fs::write(root.path().join("input"), "mutated-host-data")?;
        let descriptor = json!({"op":"exec","args":{"tree":tree["$ref"],"program":"/bin/busybox","args":["cat","/input"]}});
        let result = runtime.perform(descriptor.clone(), "actor-one", 0).await?;
        assert_eq!(result["code"], json!(0), "{result}");
        assert_eq!(result["stdout"], json!("snapshot-data"));
        assert_eq!(result, runtime.perform(descriptor, "actor-two", 0).await?);
        Ok(())
    }
}
