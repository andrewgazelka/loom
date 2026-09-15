//! Minimal libkrun 1.19 C ABI boundary, loaded only inside the expendable runner.
use anyhow::{Context, Result, bail, ensure};
use libloading::Library;
use loom_proto::VmSpec;
use std::{
    ffi::{CString, c_char},
    os::unix::ffi::OsStrExt,
    path::Path,
};

type Create = unsafe extern "C" fn() -> i32;
type Free = unsafe extern "C" fn(u32) -> i32;
type Configure = unsafe extern "C" fn(u32, u8, u32) -> i32;
type SetPath = unsafe extern "C" fn(u32, *const c_char) -> i32;
type SetEnv = unsafe extern "C" fn(u32, *const *const c_char) -> i32;
type Enter = unsafe extern "C" fn(u32) -> i32;

struct Api {
    log: unsafe extern "C" fn(u32) -> i32,
    create: Create,
    free: Free,
    configure: Configure,
    root: SetPath,
    env: SetEnv,
    enter: Enter,
    // The loaded library must outlive every copied function pointer and context.
    _library: Library,
}
impl Api {
    fn load(path: &Path) -> Result<Self> {
        // SAFETY: the host config pins this executable library (guest messages
        // cannot choose it). Signatures match libkrun 1.19.0's libkrun.h. The
        // Library stays owned by Api until all contexts and function calls end.
        unsafe {
            let library = Library::new(path).context("load host-configured libkrun")?;
            Ok(Self {
                log: *library.get(b"krun_set_log_level\0")?,
                create: *library.get(b"krun_create_ctx\0")?,
                free: *library.get(b"krun_free_ctx\0")?,
                configure: *library.get(b"krun_set_vm_config\0")?,
                root: *library.get(b"krun_set_root\0")?,
                env: *library.get(b"krun_set_env\0")?,
                enter: *library.get(b"krun_start_enter\0")?,
                _library: library,
            })
        }
    }
}
struct ContextHandle<'a> {
    api: &'a Api,
    id: u32,
}
impl Drop for ContextHandle<'_> {
    fn drop(&mut self) {
        // SAFETY: this is the uniquely owned context created through this live
        // Api. Successful start_enter exits the process and never runs Drop.
        let result = unsafe { (self.api.free)(self.id) };
        if result < 0 {
            eprintln!(
                "libkrun context cleanup failed: {}",
                std::io::Error::from_raw_os_error(result.saturating_neg())
            );
        }
    }
}
fn check(operation: &str, result: i32) -> Result<()> {
    ensure!(
        result >= 0,
        "{operation}: {}",
        std::io::Error::from_raw_os_error(result.saturating_neg())
    );
    Ok(())
}
/// Keep ordering explicit: older libkrun init parsers scan every JSON token,
/// so scalar field values must not be mistaken for subsequent field names.
#[derive(serde::Serialize)]
struct GuestConfig<'a> {
    #[serde(rename = "WorkingDir")]
    cwd: &'a str,
    #[serde(rename = "Env")]
    env: Vec<String>,
    #[serde(rename = "Cmd")]
    args: Vec<&'a str>,
}
fn write_config(root: &Path, spec: &VmSpec) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let config = GuestConfig {
        cwd: &spec.cwd,
        env: spec
            .env
            .iter()
            .map(|(key, value)| format!("{key}={value}"))
            .collect(),
        args: std::iter::once(spec.command.as_str())
            .chain(spec.args.iter().map(String::as_str))
            .collect(),
    };
    // This reserved path is newly created in the private copy. Never overwrite
    // an image entry or follow a guest-controlled symlink on the host.
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .custom_flags(libc::O_NOFOLLOW)
        .mode(0o600)
        .open(root.join(".loom-vm-launch.json"))
        .context("reserved VM launch config path already exists or is unavailable")?;
    serde_json::to_writer(file, &config).context("write guest launch config")
}
pub fn enter(library: &Path, root: &Path, spec: &VmSpec) -> Result<()> {
    let api = Api::load(library)?;
    write_config(root, spec)?;
    let root = CString::new(root.as_os_str().as_bytes())?;
    let config_env = CString::new("KRUN_CONFIG=/.loom-vm-launch.json")?;
    // libkrun 1.19 constructs a MAX_ARGS-sized Rust slice before scanning its
    // NULL sentinel. Supply all 4096 pointer slots to satisfy that ABI read.
    let mut env = vec![std::ptr::null(); 4096];
    env[0] = config_env.as_ptr();
    let cpus = u8::try_from(spec.cpus).context("VM CPU count exceeds libkrun ABI")?;
    let memory = u32::try_from(spec.memory_mb).context("VM memory exceeds libkrun ABI")?;
    // SAFETY: loaded signatures match the pinned header. All buffers above are
    // NUL-terminated, pointer lists have a NULL sentinel (including empty env),
    // and their owners plus Library outlive every call including start_enter.
    unsafe {
        check("krun_set_log_level", (api.log)(1))?;
        let id = (api.create)();
        check("krun_create_ctx", id)?;
        let context = ContextHandle {
            api: &api,
            id: u32::try_from(id)?,
        };
        check(
            "krun_set_vm_config",
            (api.configure)(context.id, cpus, memory),
        )?;
        check("krun_set_root", (api.root)(context.id, root.as_ptr()))?;
        check("krun_set_env", (api.env)(context.id, env.as_ptr()))?;
        let result = (api.enter)(context.id);
        check("krun_start_enter", result)?;
        bail!(
            "krun_start_enter unexpectedly returned {result}; successful VM execution must exit with its guest status"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn spec() -> VmSpec {
        serde_json::from_value(serde_json::json!({
            "image":{"$ref":"test-image"},"command":"/bin/命令","cwd":"/目录",
            "args":["","two words","quote\"back\\slash","雪😀\n\u{1}","WorkingDir","Env"],
            "env":{"VALUE":"雪😀\n\u{1}","KRUN_INIT":"literal-user-value","KRUN_WORKDIR":"another-user-value"}
        })).unwrap()
    }
    #[test]
    fn config_preserves_unicode_empty_arguments_and_literal_control_environment() {
        let root = tempfile::tempdir().unwrap();
        let spec = spec();
        write_config(root.path(), &spec).unwrap();
        let config: serde_json::Value = serde_json::from_slice(
            &std::fs::read(root.path().join(".loom-vm-launch.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(config["WorkingDir"], spec.cwd);
        let args: Vec<String> = serde_json::from_value(config["Cmd"].clone()).unwrap();
        assert_eq!(
            args,
            std::iter::once(spec.command)
                .chain(spec.args)
                .collect::<Vec<_>>()
        );
        let env: Vec<String> = serde_json::from_value(config["Env"].clone()).unwrap();
        assert_eq!(
            env,
            spec.env
                .iter()
                .map(|(key, value)| format!("{key}={value}"))
                .collect::<Vec<_>>()
        );
    }
    #[test]
    fn config_refuses_guest_symlinks_and_existing_image_entries() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("original");
        std::fs::write(&target, b"unchanged").unwrap();
        let config = root.path().join(".loom-vm-launch.json");
        std::os::unix::fs::symlink(&target, &config).unwrap();
        assert!(write_config(root.path(), &spec()).is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"unchanged");
        std::fs::remove_file(&config).unwrap();
        std::fs::write(&config, b"image-entry").unwrap();
        assert!(write_config(root.path(), &spec()).is_err());
        assert_eq!(std::fs::read(&config).unwrap(), b"image-entry");
    }
}
