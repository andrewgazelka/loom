use crate::filesystem::{self, PinnedRoot, RootIdentity, WalkLimits};
use crate::{Runtime, required_str};
use anyhow::{Context, Result, bail, ensure};
#[cfg(test)]
use loom_proto::DirEntry;
use loom_proto::{Actor, EntryKind, Lang, Tree, TreeEntry, Value};
use serde_json::json;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

impl Runtime {
    pub fn create_machine(&self, path: &Path) -> Result<Actor> {
        let root = Arc::new(PinnedRoot::open(path)?);
        let actor = Actor {
            id: uuid::Uuid::new_v4().to_string(),
            behavior_hash: "loom:machine".into(),
            lang: Lang::Rust,
            component_hash: None,
            last_seq: 0,
            created_seq: 0,
            parent: None,
        };
        let actor = self.inner.store.create_initialized_actor(
            &actor,
            &json!({"root":root.path,"identity":root.identity}),
        )?;
        self.inner
            .machine_roots
            .lock()
            .map_err(|_| anyhow::anyhow!("machine root cache poisoned"))?
            .insert(actor.id.clone(), root);
        Ok(actor)
    }
    fn machine_handle(&self, id: &str) -> Result<Arc<PinnedRoot>> {
        let mut roots = self
            .inner
            .machine_roots
            .lock()
            .map_err(|_| anyhow::anyhow!("machine root cache poisoned"))?;
        if let Some(root) = roots.get(id) {
            return Ok(root.clone());
        }
        let actor = self
            .inner
            .store
            .actor(id)?
            .context("machine actor not found")?;
        ensure!(
            actor.behavior_hash == "loom:machine",
            "actor is not a machine"
        );
        let mut state = self
            .inner
            .store
            .latest_snapshot(id, "loom:machine")?
            .context("machine state missing")?
            .state;
        let root = Arc::new(PinnedRoot::open(Path::new(required_str(&state, "root")?))?);
        if let Some(identity) = state.get("identity") {
            let expected: RootIdentity = serde_json::from_value(identity.clone())?;
            ensure!(
                root.identity == expected,
                "machine root identity changed; register the replacement as a new machine"
            );
        } else {
            // Path-only legacy machines are pinned once and persisted before use.
            state["identity"] = serde_json::to_value(root.identity)?;
            self.inner.store.pin_machine_root(id, &state)?;
        }
        roots.insert(id.into(), root.clone());
        Ok(root)
    }
    pub fn machine_root(&self, id: &str) -> Result<PathBuf> {
        Ok(self.machine_handle(id)?.path.clone())
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
                capture_paths: super::capture_paths(&args)?,
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
        let root = self.machine_handle(required_str(args, "machine")?)?;
        ensure!(
            root.path == Path::new("/"),
            "process paths require an unrestricted root machine"
        );
        let requested = required_str(args, "path")?;
        let _directory = root.directory(requested)?;
        Ok(root.path.join(filesystem::relative_path(requested)?))
    }
    pub(crate) async fn list_machine_directory(&self, args: &Value) -> Result<crate::EffectOutput> {
        let root = self.machine_handle(required_str(args, "machine")?)?;
        let path = required_str(args, "path")?.to_owned();
        let entries = tokio::task::spawn_blocking(move || root.list(&path, 100_000)).await??;
        Ok(crate::EffectOutput {
            bytes: loom_proto::encode_host(&entries).map_err(anyhow::Error::msg)?,
        })
    }
    pub(crate) async fn walk_machine_directory(&self, args: &Value) -> Result<crate::EffectOutput> {
        fn limit(args: &Value, name: &str, default: u64) -> Result<u64> {
            args.get(name).map_or(Ok(default), |value| {
                value
                    .as_u64()
                    .with_context(|| format!("{name} must be a nonnegative integer"))
            })
        }
        let root = self.machine_handle(required_str(args, "machine")?)?;
        let path = required_str(args, "path")?.to_owned();
        let max_depth = u32::try_from(limit(args, "max_depth", 64)?)?;
        let max_entries = usize::try_from(limit(args, "max_entries", 100_000)?)?;
        let entries = root
            .walk(
                &path,
                WalkLimits {
                    max_depth,
                    max_entries,
                },
            )
            .await?;
        Ok(crate::EffectOutput {
            bytes: loom_proto::encode_host(&entries).map_err(anyhow::Error::msg)?,
        })
    }
    pub(crate) async fn stat_machine_path(&self, args: &Value) -> Result<Value> {
        let root = self.machine_handle(required_str(args, "machine")?)?;
        let path = required_str(args, "path")?.to_owned();
        let entry = tokio::task::spawn_blocking(move || root.stat(&path)).await??;
        Ok(serde_json::to_value(entry)?)
    }
    pub(crate) async fn read_machine_file(&self, args: &Value) -> Result<Value> {
        let root = self.machine_handle(required_str(args, "machine")?)?;
        let path = required_str(args, "path")?.to_owned();
        let bytes = tokio::task::spawn_blocking(move || root.read(&path)).await??;
        Ok(json!(String::from_utf8(bytes).context(
            "file is not UTF8; use snapshot for binary data"
        )?))
    }
    pub(crate) async fn read_optional_machine_file(&self, args: &Value) -> Result<Value> {
        let root = self.machine_handle(required_str(args, "machine")?)?;
        let path = required_str(args, "path")?.to_owned();
        let bytes = tokio::task::spawn_blocking(move || root.read_optional(&path)).await??;
        let content = bytes
            .map(String::from_utf8)
            .transpose()
            .context("file is not UTF8; use snapshot for binary data")?;
        Ok(json!(content))
    }

    pub(crate) async fn write_machine_file(&self, args: &Value) -> Result<Value> {
        let root = self.machine_handle(required_str(args, "machine")?)?;
        let path = required_str(args, "path")?.to_owned();
        let content = required_str(args, "content")?.to_owned();
        tokio::task::spawn_blocking(move || root.write(&path, content.as_bytes())).await??;
        Ok(Value::Null)
    }

    pub(crate) async fn snapshot_tree(&self, args: &Value) -> Result<Value> {
        let root = self.machine_handle(required_str(args, "machine")?)?;
        let path = required_str(args, "path")?.to_owned();
        let store = self.inner.store.clone();
        let hash = tokio::task::spawn_blocking(move || {
            snapshot(&store, &root.directory(&path)?, &mut 0, 0)
        })
        .await??;
        self.inner
            .store
            .reference(&hash, loom_proto::DAG_CBOR_CODEC)
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
        self.execute_command(command, Vec::new()).await
    }
}
fn snapshot(
    store: &loom_store::Store,
    directory: &std::fs::File,
    count: &mut usize,
    depth: usize,
) -> Result<String> {
    use std::os::unix::fs::PermissionsExt;
    ensure!(depth < 128, "snapshot depth limit exceeded");
    let mut entries = Vec::new();
    for entry in filesystem::list_directory(directory, 100_000)? {
        *count += 1;
        ensure!(*count <= 100_000, "snapshot entry limit exceeded");
        ensure!(
            entry.kind == EntryKind::File || entry.kind == EntryKind::Directory,
            "snapshot requires regular files and directories; symlinks are not supported"
        );
        let child =
            filesystem::open_at(directory, &entry.name, entry.kind == EntryKind::Directory)?;
        let metadata = child.metadata()?;
        let is_directory = metadata.is_dir();
        let executable = metadata.permissions().mode() & 0o111 != 0;
        let hash = if is_directory {
            snapshot(store, &child, count, depth + 1)?
        } else {
            store.put("blob", &filesystem::read_regular(child)?)?
        };
        entries.push(TreeEntry {
            name: entry.name,
            reference: store.reference(
                &hash,
                if is_directory {
                    loom_proto::DAG_CBOR_CODEC
                } else {
                    loom_proto::RAW_CODEC
                },
            )?,
            directory: is_directory,
            executable,
        });
    }
    store.put_value("tree", &Tree { entries })
}
fn materialize(store: &loom_store::Store, hash: &str, path: &Path, depth: usize) -> Result<()> {
    ensure!(depth < 128, "tree depth limit exceeded");
    let tree: Tree = store.get_value(hash)?.context("tree missing")?;
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
            materialize(
                store,
                required_str(&entry.reference, "$ref")?,
                &target,
                depth + 1,
            )?;
        } else {
            std::fs::write(
                &target,
                store
                    .get(required_str(&entry.reference, "$ref")?)?
                    .context("tree blob missing")?,
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
    #[cfg(unix)]
    #[tokio::test]
    async fn directory_listing_classifies_links_without_following_them() -> Result<()> {
        use std::os::unix::{fs::symlink, net::UnixListener};
        let root = tempfile::tempdir()?;
        let outside = tempfile::tempdir()?;
        std::fs::create_dir(root.path().join("sub"))?;
        std::fs::write(root.path().join("small"), b"123")?;
        std::fs::write(root.path().join("sub/largest"), b"123456789")?;
        std::fs::File::create(outside.path().join("huge"))?.set_len(1_000_000)?;
        symlink(outside.path(), root.path().join("external-dir"))?;
        symlink(
            outside.path().join("huge"),
            root.path().join("external-file"),
        )?;
        symlink(root.path(), root.path().join("sub/cycle"))?;
        let _socket = UnixListener::bind(root.path().join("socket"))?;
        let runtime = Runtime::new(loom_store::Store::memory()?)?;
        let machine = runtime.create_machine(root.path())?;
        let top = runtime
            .perform(
                json!({"op":"fs.list","args":{"machine":machine.id,"path":"/"}}),
                "classification",
                0,
            )
            .await?;
        let top: Vec<DirEntry> = serde_json::from_value(top)?;
        let names = top
            .iter()
            .map(|entry| entry.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            vec!["external-dir", "external-file", "small", "socket", "sub"]
        );
        for name in ["external-dir", "external-file"] {
            let entry = top.iter().find(|entry| entry.name == name).unwrap();
            assert_eq!(entry.kind, EntryKind::Symlink);
            assert_eq!(entry.size, 0);
        }
        let socket = top.iter().find(|entry| entry.name == "socket").unwrap();
        assert_eq!(socket.kind, EntryKind::Other);
        assert_eq!(socket.size, 0);
        struct FoundFile {
            path: String,
            size: u64,
        }
        let mut largest: Option<FoundFile> = None;
        let mut pending = vec!["/".to_owned()];
        let mut directories = 0;
        while let Some(path) = pending.pop() {
            directories += 1;
            assert!(
                directories <= 2,
                "followed a cyclic or external directory link"
            );
            let entries = runtime
                .perform(
                    json!({"op":"fs.list","args":{"machine":machine.id,"path":path}}),
                    &format!("traversal:{directories}"),
                    0,
                )
                .await?;
            let entries: Vec<DirEntry> = serde_json::from_value(entries)?;
            for entry in entries {
                let path = format!("{}/{}", path.trim_end_matches('/'), entry.name);
                if entry.kind == EntryKind::Directory {
                    pending.push(path);
                } else if entry.kind == EntryKind::File {
                    let size = entry.size;
                    if largest.as_ref().is_none_or(|file| size > file.size) {
                        largest = Some(FoundFile { path, size });
                    }
                }
            }
        }
        let largest = largest.context("regular file missing")?;
        assert_eq!(largest.path, "/sub/largest");
        assert_eq!(largest.size, 9);
        assert_eq!(directories, 2);
        let walked = runtime
            .perform(
                json!({"op":"fs.walk","args":{"machine":machine.id,"path":"/","max_depth":4,"max_entries":20}}),
                "walk",
                0,
            )
            .await?;
        let walked: Vec<DirEntry> = serde_json::from_value(walked)?;
        assert_eq!(walked.len(), 7);
        assert!(walked.windows(2).all(|pair| pair[0].name < pair[1].name));
        assert!(
            walked
                .iter()
                .any(|entry| entry.name == "sub/largest" && entry.size == 9)
        );
        assert!(
            runtime
                .perform(
                    json!({"op":"fs.list","args":{"machine":machine.id,"path":"/external-dir"}}),
                    "escape",
                    0
                )
                .await
                .is_err()
        );
        Ok(())
    }
    #[tokio::test]
    async fn optional_reads_and_writes_use_the_pinned_machine() -> Result<()> {
        let workspace = tempfile::tempdir()?;
        let path = workspace.path().join("root");
        std::fs::create_dir(&path)?;
        let runtime = Runtime::new(loom_store::Store::memory()?)?;
        let machine = runtime.create_machine(&path)?;
        let args = json!({"machine":machine.id,"path":"new.txt","content":"héllo"});
        assert_eq!(
            runtime.read_optional_machine_file(&args).await?,
            Value::Null
        );
        assert!(
            !path.join("new.txt").exists(),
            "preview must not create files"
        );
        assert_eq!(runtime.write_machine_file(&args).await?, Value::Null);
        assert_eq!(
            runtime.read_optional_machine_file(&args).await?,
            json!("héllo")
        );
        let mut replacement = args.clone();
        replacement["content"] = json!("short");
        runtime.write_machine_file(&replacement).await?;
        assert_eq!(runtime.read_machine_file(&args).await?, json!("short"));
        std::fs::create_dir(path.join("nested"))?;
        runtime
            .write_machine_file(
                &json!({"machine":machine.id,"path":"nested/file","content":"nested"}),
            )
            .await?;
        assert_eq!(std::fs::read_to_string(path.join("nested/file"))?, "nested");
        assert!(!std::fs::read_dir(&path)?.any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".loom-write-")
        }));
        std::fs::rename(&path, workspace.path().join("moved"))?;
        std::fs::create_dir(&path)?;
        runtime.write_machine_file(&args).await?;
        assert_eq!(
            std::fs::read_to_string(workspace.path().join("moved/new.txt"))?,
            "héllo"
        );
        assert!(!path.join("new.txt").exists());
        Ok(())
    }

    #[tokio::test]
    async fn optional_reads_refuse_invalid_targets_and_writes_do_not_follow_links() -> Result<()> {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir()?;
        let outside = tempfile::tempdir()?;
        std::fs::write(outside.path().join("data"), "untouched")?;
        std::fs::write(root.path().join("binary"), [255])?;
        std::fs::create_dir(root.path().join("directory"))?;
        symlink(outside.path(), root.path().join("link-dir"))?;
        symlink(outside.path().join("data"), root.path().join("link-file"))?;
        symlink(outside.path().join("absent"), root.path().join("dangling"))?;
        std::fs::hard_link(outside.path().join("data"), root.path().join("hardlink"))?;
        let runtime = Runtime::new(loom_store::Store::memory()?)?;
        let machine = runtime.create_machine(root.path())?;
        for path in [
            "../escape",
            "link-dir/data",
            "link-file",
            "dangling",
            "directory",
            "missing/child",
            ".",
        ] {
            let args = json!({"machine":machine.id,"path":path,"content":"changed"});
            assert!(
                runtime.read_optional_machine_file(&args).await.is_err(),
                "{path}"
            );
            assert!(runtime.write_machine_file(&args).await.is_err(), "{path}");
        }
        assert!(
            runtime
                .read_optional_machine_file(&json!({"machine":machine.id,"path":"binary"}))
                .await
                .is_err()
        );
        runtime
            .write_machine_file(
                &json!({"machine":machine.id,"path":"hardlink","content":"changed"}),
            )
            .await?;
        assert_eq!(
            std::fs::read_to_string(outside.path().join("data"))?,
            "untouched"
        );
        assert_eq!(
            std::fs::read_to_string(root.path().join("hardlink"))?,
            "changed"
        );
        assert!(
            runtime
                .write_machine_file(&json!({"machine":"unknown","path":"new","content":"bad"}))
                .await
                .is_err()
        );
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn directory_listing_rejects_unrepresentable_names() -> Result<()> {
        use std::os::unix::ffi::OsStringExt;
        let root = tempfile::tempdir()?;
        std::fs::File::create(root.path().join(std::ffi::OsString::from_vec(vec![255])))?;
        let runtime = Runtime::new(loom_store::Store::memory()?)?;
        let machine = runtime.create_machine(root.path())?;
        let error = runtime
            .list_machine_directory(&json!({"machine":machine.id,"path":"/"}))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("UTF8"));
        Ok(())
    }
    #[tokio::test]
    async fn machine_pin_survives_rename_and_restart_rejects_replacement() -> Result<()> {
        let workspace = tempfile::tempdir()?;
        let root = workspace.path().join("root");
        std::fs::create_dir(&root)?;
        std::fs::write(root.join("data"), "original")?;
        let db = workspace.path().join("state.sqlite");
        let runtime = Runtime::new(loom_store::Store::open(&db)?)?;
        let machine = runtime.create_machine(&root)?;
        let args = json!({"machine":machine.id,"path":"data"});
        let tree_args = json!({"machine":machine.id,"path":"/"});
        let original_tree = runtime.snapshot_tree(&tree_args).await?;
        std::fs::rename(&root, workspace.path().join("moved"))?;
        std::fs::create_dir(&root)?;
        std::fs::write(root.join("data"), "replacement")?;
        assert_eq!(runtime.read_machine_file(&args).await?, json!("original"));
        assert_eq!(runtime.snapshot_tree(&tree_args).await?, original_tree);
        drop(runtime);
        let store = loom_store::Store::open(&db)?;
        store.rebuild_views()?;
        let restarted = Runtime::new(store)?;
        let error = restarted.read_machine_file(&args).await.unwrap_err();
        assert!(error.to_string().contains("identity changed"), "{error:#}");
        Ok(())
    }

    #[test]
    fn legacy_machine_pin_is_migrated_and_rebuilt() -> Result<()> {
        let root = tempfile::tempdir()?;
        let store = loom_store::Store::memory()?;
        let actor = Actor {
            id: uuid::Uuid::new_v4().to_string(),
            behavior_hash: "loom:machine".into(),
            lang: Lang::Rust,
            component_hash: None,
            last_seq: 0,
            created_seq: 0,
            parent: None,
        };
        store.create_initialized_actor(&actor, &json!({"root":root.path()}))?;
        let runtime = Runtime::new(store.clone())?;
        assert_eq!(
            runtime.machine_root(&actor.id)?,
            root.path().canonicalize()?
        );
        store.rebuild_views()?;
        let snapshot = store
            .latest_snapshot(&actor.id, "loom:machine")?
            .context("snapshot")?;
        assert!(snapshot.state.get("identity").is_some());
        let reopened = Runtime::new(store)?;
        assert_eq!(
            reopened.machine_root(&actor.id)?,
            root.path().canonicalize()?
        );
        Ok(())
    }

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
