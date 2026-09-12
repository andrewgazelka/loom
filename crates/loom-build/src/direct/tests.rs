use super::*;
#[tokio::test]
async fn host_execution_is_admitted_before_build_scripts_run() {
    let directory =
        std::env::temp_dir().join(format!("loom-host-admission-{}", std::process::id()));
    std::fs::create_dir_all(directory.join("src")).unwrap();
    std::fs::write(
        directory.join("Cargo.toml"),
        "[package]\nname='unreviewed-host-code'\nversion='0.1.0'\nedition='2024'\n[workspace]\n",
    )
    .unwrap();
    std::fs::write(
        directory.join("Cargo.lock"),
        "version = 4\n[[package]]\nname = \"unreviewed-host-code\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    std::fs::write(directory.join("src/lib.rs"), "pub fn value() -> u8 { 1 }").unwrap();
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap();
    assert!(
        graph_shareable(
            root,
            &directory,
            &directory,
            &directory.join("target"),
            false,
            None,
        )
        .await
        .unwrap()
    );
    std::fs::write(
        directory.join("build.rs"),
        "fn main() { std::fs::write(\"executed\", b\"bad\").unwrap(); }",
    )
    .unwrap();
    assert!(
        graph_shareable(
            root,
            &directory,
            &directory,
            &directory.join("target"),
            false,
            None,
        )
        .await
        .is_err()
    );
    assert!(!directory.join("executed").exists());
    let store = Store::memory().unwrap();
    artifacts::initialize_index(&store).unwrap();
    let key = "ab".repeat(32);
    let mut unit = artifacts::Unit {
        key: key.clone(),
        name: "known_dependency".into(),
        recipe: Recipe::parse(b"LOOM_RUSTC_ARGUMENTS\0rustc\0", &directory).unwrap(),
        outputs: vec![artifacts::Output {
            path: directory.join("dependency.rlib"),
            hash: store
                .put("rust-artifact", b"original compiler output")
                .unwrap(),
            executable: false,
        }],
        dependencies: Vec::new(),
    };
    artifacts::publish_units(&store, true, &[unit.clone()]).unwrap();
    let original = store
        .with_connection(|connection| {
            Ok(connection.query_row(
                "SELECT artifact_hash FROM rust_artifacts WHERE key=?",
                [&key],
                |row| row.get::<_, String>(0),
            )?)
        })
        .unwrap();
    unit.outputs[0].hash = store
        .put("rust-artifact", b"poisoned sibling output")
        .unwrap();
    let admitted = graph_shareable(
        root,
        &directory,
        &directory,
        &directory.join("target"),
        false,
        None,
    )
    .await
    .unwrap_or(false);
    artifacts::publish_units(&store, admitted, &[unit]).unwrap();
    let after = store
        .with_connection(|connection| {
            Ok(connection.query_row(
                "SELECT artifact_hash FROM rust_artifacts WHERE key=?",
                [&key],
                |row| row.get::<_, String>(0),
            )?)
        })
        .unwrap();
    assert_eq!(
        after, original,
        "unreviewed host code replaced a shared artifact"
    );
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn root_edits_reuse_incremental_state_and_execute_new_code() {
    fn run(recipe: &Recipe) -> std::process::Output {
        std::process::Command::new(&recipe.compiler)
            .args(&recipe.arguments)
            .envs(&recipe.environment)
            .current_dir(recipe.working_directory())
            .output()
            .unwrap()
    }
    let directory =
        std::env::temp_dir().join(format!("loom-root-incremental-{}", std::process::id()));
    let old = directory.join("old");
    let new = directory.join("new");
    for source in [&old, &new] {
        std::fs::create_dir_all(source).unwrap();
        std::fs::write(
            source.join("Cargo.toml"),
            "[package]\nname='incremental-control'\nversion='0.1.0'\n",
        )
        .unwrap();
    }
    let unchanged = (0..128)
        .map(|index| {
            format!(
                "pub fn f{index}(x:u64)->u64{{x.wrapping_mul({})}}\n",
                index + 2
            )
        })
        .collect::<String>();
    std::fs::write(
        old.join("lib.rs"),
        format!("pub fn changed()->u64{{1}}\n{unchanged}"),
    )
    .unwrap();
    std::fs::write(
        new.join("lib.rs"),
        format!("pub fn changed()->u64{{2}}\n{unchanged}"),
    )
    .unwrap();
    let workspace = directory.join("workspace");
    materialize_root_workspace(&old, &workspace).unwrap();
    let output = directory.join("cold-output");
    std::fs::create_dir_all(&output).unwrap();
    let incremental = directory.join("incremental");
    let compiler = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".into());
    let capture = format!(
        "LOOM_RUSTC_CWD={}\0RUSTC_BOOTSTRAP=1\0LOOM_RUSTC_ARGUMENTS\0{compiler}\0",
        workspace.display()
    );
    let mut recipe = Recipe::parse(capture.as_bytes(), &workspace).unwrap();
    recipe.arguments = vec![
        "--edition=2024".into(),
        "--crate-name".into(),
        "loom_definition".into(),
        "--crate-type".into(),
        "rlib".into(),
        "lib.rs".into(),
        "--out-dir".into(),
        output.to_string_lossy().into_owned(),
        "-Copt-level=2".into(),
        "-Ccodegen-units=16".into(),
        "-C".into(),
        format!("incremental={}", incremental.display()),
        "-Funsafe-code".into(),
        format!("--remap-path-prefix={}=/loom/build", workspace.display()),
        format!("--remap-path-prefix={}=/loom/source", workspace.display()),
        "-Zincremental-info".into(),
    ];
    let first = run(&recipe);
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    materialize_root_workspace(&new, &workspace).unwrap();
    let warm = directory.join("warm-output");
    recipe.relocate(&workspace, &warm, &incremental).unwrap();
    let second = run(&recipe);
    let diagnostics = String::from_utf8_lossy(&second.stderr);
    assert!(second.status.success(), "{diagnostics}");
    assert!(
        !diagnostics.contains("completely ignoring cache"),
        "{diagnostics}"
    );
    assert!(
        diagnostics
            .lines()
            .any(|line| line.contains("files hard-linked")
                && line
                    .split_whitespace()
                    .nth(3)
                    .and_then(|count| count.parse::<usize>().ok())
                    .is_some_and(|count| count > 0)),
        "{diagnostics}"
    );
    let main = directory.join("main.rs");
    std::fs::write(
        &main,
        "fn main(){assert_eq!(loom_definition::changed(),2);}",
    )
    .unwrap();
    let executable = directory.join("witness");
    let linked = std::process::Command::new(&compiler)
        .arg("--edition=2024")
        .arg(&main)
        .arg("--extern")
        .arg(format!(
            "loom_definition={}",
            warm.join("libloom_definition.rlib").display()
        ))
        .arg("-o")
        .arg(&executable)
        .output()
        .unwrap();
    assert!(
        linked.status.success(),
        "{}",
        String::from_utf8_lossy(&linked.stderr)
    );
    assert!(
        std::process::Command::new(executable)
            .status()
            .unwrap()
            .success()
    );
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn dependency_replay_preserves_compiler_working_directory() {
    let directory = std::env::temp_dir().join(format!("loom-compiler-cwd-{}", std::process::id()));
    let package = directory.join("deps/a package");
    std::fs::create_dir_all(&package).unwrap();
    std::fs::write(package.join("input"), b"correct compiler cwd").unwrap();
    let capture = format!(
        "LOOM_RUSTC_CWD={}\0LOOM_RUSTC_ARGUMENTS\0cat\0deps/a package/input\0",
        directory.display()
    );
    let recipe = Recipe::parse(capture.as_bytes(), &package).unwrap();
    let output = std::process::Command::new("/bin/sh")
        .arg("-c")
        .arg(recipe.shell())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"correct compiler cwd");
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn capture_preserves_spaces_and_package_values() {
    let recipe = Recipe::parse(b"CARGO_PKG_DESCRIPTION=a = b\0LOOM_RUSTC_ARGUMENTS\0/opt/rust c\0--crate-name\0loom_definition\0--out-dir\0/a b\0", Path::new("/old")).unwrap();
    assert_eq!(recipe.environment["CARGO_PKG_DESCRIPTION"], "a = b");
    assert_eq!(recipe.compiler, "/opt/rust c");
    assert_eq!(
        recipe.output().unwrap(),
        Path::new("/a b/loom_definition.wasm")
    );
}
#[test]
fn direct_compiler_errors_become_checker_diagnostics() {
    let messages = rustc_diagnostics(
        "warning\n{\"level\":\"error\",\"message\":\"bad type\",\"code\":{\"code\":\"E0308\"},\"spans\":[],\"children\":[]}\n",
    );
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].code, "E0308");
}
#[test]
fn corrupt_materialization_is_replaced_from_cas() {
    let store = Store::memory().unwrap();
    let directory = std::env::temp_dir().join(format!(
        "loom-artifact-integrity-01a0866a-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join("dependency.rlib");
    let hash = store.put("rust-artifact", b"verified artifact").unwrap();
    std::fs::write(&path, b"corrupted artifact").unwrap();
    let mut recipe = Recipe::parse(b"LOOM_RUSTC_ARGUMENTS\0rustc\0", &directory).unwrap();
    recipe.artifacts.insert(
        path.clone(),
        ArtifactFile {
            hash,
            executable: false,
        },
    );
    assert!(recipe.restore_artifacts(&store).unwrap());
    assert_eq!(std::fs::read(path).unwrap(), b"verified artifact");
    std::fs::remove_dir_all(directory).unwrap();
}

#[cfg(unix)]
#[test]
fn restored_build_script_retains_execute_permission() {
    use std::os::unix::fs::PermissionsExt;
    let store = Store::memory().unwrap();
    let directory = std::env::temp_dir().join(format!(
        "loom-artifact-executable-01a0866a-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join("build-script-build");
    let hash = store
        .put("rust-artifact", b"compiled build script")
        .unwrap();
    std::fs::write(&path, b"compiled build script").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    let mut recipe = Recipe::parse(b"LOOM_RUSTC_ARGUMENTS\0rustc\0", &directory).unwrap();
    recipe.artifacts.insert(
        path.clone(),
        ArtifactFile {
            hash,
            executable: true,
        },
    );
    assert!(recipe.restore_artifacts(&store).unwrap());
    assert_ne!(
        std::fs::metadata(path).unwrap().permissions().mode() & 0o111,
        0
    );
    std::fs::remove_dir_all(directory).unwrap();
}
