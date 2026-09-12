//! Replay Cargo's actual root-crate invocations, holding dependency artifacts fixed.
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

#[derive(Serialize, Deserialize)]
struct Invocation {
    args: Vec<String>,
    env: BTreeMap<String, String>,
    cwd: PathBuf,
}
struct Fixture {
    name: &'static str,
    source: &'static str,
    before: &'static str,
    after: &'static str,
}
struct Sample {
    wall_ms: f64,
    stats: String,
    phases: BTreeMap<String, f64>,
}
fn cargo_compile_variable(name: &str) -> bool {
    name.starts_with("CARGO_PKG_")
        || name.starts_with("CARGO_MANIFEST_")
        || name.starts_with("CARGO_FEATURE_")
        || name.starts_with("CARGO_CFG_")
        || name.starts_with("DEP_")
        || matches!(
            name,
            "CARGO_CRATE_NAME" | "OUT_DIR" | "LD_LIBRARY_PATH" | "DYLD_FALLBACK_LIBRARY_PATH"
        )
}
fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        if entry.file_name() == "target" {
            continue;
        }
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &to.join(entry.file_name()));
        } else {
            std::fs::copy(entry.path(), to.join(entry.file_name())).unwrap();
        }
    }
}
fn compile(driver: &Path, invocation: &Invocation, output: &Path, cache: Option<&Path>) -> Sample {
    std::fs::create_dir_all(output).unwrap();
    let mut args = invocation.args.clone();
    let position = args.iter().position(|arg| arg == "--out-dir").unwrap();
    args[position + 1] = output.to_str().unwrap().into();
    let mut command = Command::new(driver);
    command
        .args(args)
        .current_dir(&invocation.cwd)
        .envs(&invocation.env)
        .env_remove("LOOM_OBJECT_CACHE")
        .env_remove("LOOM_ITEM_HASHES")
        .env_remove("LOOM_ITEM_PREIMAGES")
        .env_remove("LOOM_OBJECT_CACHE_FAIL_PUBLISH")
        .env("LOOM_OBJECT_CACHE_STATS", "1")
        .env("LOOM_OBJECT_CACHE_TIMINGS", "1");
    if let Some(cache) = cache {
        command.env("LOOM_OBJECT_CACHE", cache);
    }
    let start = Instant::now();
    let result = command.output().unwrap();
    let wall_ms = start.elapsed().as_secs_f64() * 1000.;
    std::fs::write(output.join("stderr.log"), &result.stderr).unwrap();
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(result.status.success(), "{}: {stderr}", output.display());
    for line in stderr
        .lines()
        .filter(|line| line.starts_with("object-cache: bypass"))
    {
        println!("{line}");
    }
    let stats = stderr
        .lines()
        .find(|line| line.starts_with("object-cache: cgus="))
        .unwrap_or("")
        .to_owned();
    let mut phases = BTreeMap::new();
    if let Some(line) = stderr
        .lines()
        .find(|line| line.starts_with("object-cache-timing: "))
    {
        for field in line.split_whitespace().skip(1) {
            let (name, value) = field.split_once('=').unwrap();
            phases.insert(name.to_owned(), value.parse().unwrap());
        }
    }
    if cache.is_some() {
        assert!(!stats.is_empty(), "missing cache statistics: {stderr}");
        assert_eq!(phases.len(), 4, "missing phase timings: {stderr}");
    }
    Sample {
        wall_ms,
        stats,
        phases,
    }
}
fn median(mut values: Vec<f64>) -> f64 {
    values.sort_by(f64::total_cmp);
    values[values.len() / 2]
}
fn main() {
    if let Some(directory) = std::env::var_os("LOOM_BENCH_CAPTURE") {
        let mut args = std::env::args().skip(1);
        let rustc = args.next().unwrap();
        let args: Vec<_> = args.collect();
        if let Some(index) = args.iter().position(|arg| arg == "--crate-name") {
            let name = &args[index + 1];
            if matches!(
                name.as_str(),
                "loom_guest_rs" | "loom_example_preview" | "loom_actor"
            ) {
                let invocation = Invocation {
                    args: args.clone(),
                    env: std::env::vars()
                        .filter(|entry| cargo_compile_variable(&entry.0))
                        .collect(),
                    cwd: std::env::current_dir().unwrap(),
                };
                std::fs::write(
                    PathBuf::from(directory).join(format!("{name}.json")),
                    serde_json::to_vec_pretty(&invocation).unwrap(),
                )
                .unwrap();
            }
        }
        std::process::exit(
            Command::new(rustc)
                .args(args)
                .status()
                .unwrap()
                .code()
                .unwrap_or(1),
        );
    }
    let mut args = std::env::args().skip(1);
    let driver = std::fs::canonicalize(args.next().expect("driver path")).unwrap();
    let root = std::fs::canonicalize(args.next().expect("persistent staging path")).unwrap();
    let mode = args.next();
    let smoke = mode.as_deref() == Some("smoke");
    let coverage = matches!(mode.as_deref(), Some("coverage" | "audit"));
    if mode.as_deref() == Some("coverage") {
        assert!(
            !root.join("workspace").exists()
                && !root.join("loom_guest_rs.json").exists()
                && !root.join("loom_example_preview.json").exists(),
            "coverage requires a fresh staging directory to audit one complete source snapshot"
        );
    }
    for name in ["loom_guest_rs", "loom_example_preview"] {
        let path = root.join(format!("{name}.json"));
        if path.exists() {
            let mut invocation: Invocation =
                serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            invocation
                .env
                .retain(|name, _| cargo_compile_variable(name));
            std::fs::write(path, serde_json::to_vec_pretty(&invocation).unwrap()).unwrap();
        }
    }
    let repo = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap();
    let workspace = root.join("workspace");
    if !root.join("loom_example_preview.json").exists() {
        let mut directories = vec![
            "crates/loom-guest-rs",
            "crates/loom-proto",
            "examples/rust-preview",
        ];
        if coverage {
            directories.push("crates/loom-actor");
        }
        for directory in directories {
            copy_tree(&repo.join(directory), &workspace.join(directory));
        }
        std::fs::copy(repo.join("Cargo.lock"), workspace.join("Cargo.lock")).unwrap();
        std::fs::write(workspace.join("Cargo.toml"), "[workspace]\nresolver = \"3\"\nmembers = [\"crates/*\", \"examples/*\"]\n[profile.release]\nopt-level = 2\ndebug = 0\nstrip = \"none\"\nlto = false\nincremental = false\n").unwrap();
        let mut cargo = Command::new("cargo");
        cargo.args(["+nightly-2026-08-24", "build"]);
        if coverage {
            cargo.args(["-p", "loom-actor"]);
        }
        let status = cargo
            .args(["--release", "-j4", "--manifest-path"])
            .arg(workspace.join("Cargo.toml"))
            .args(["-p", "loom-example-preview"])
            .env("RUSTC_WRAPPER", std::env::current_exe().unwrap())
            .env("LOOM_BENCH_CAPTURE", &root)
            .env("RUSTFLAGS", "-C lto=off -C embed-bitcode=no")
            .env_remove("LOOM_OBJECT_CACHE")
            .status()
            .unwrap();
        assert!(status.success());
    }
    let mut fixtures = vec![
        Fixture {
            name: "loom_guest_rs",
            source: "crates/loom-guest-rs/src/lib.rs",
            before: "perform(\"sleep\", serde_json::json!({\"ms\": ms}))",
            after: "perform(\"sleep\", serde_json::json!({\"ms\": ms.saturating_add(1)}))",
        },
        Fixture {
            name: "loom_example_preview",
            source: "examples/rust-preview/src/lib.rs",
            before: "intermediate\\n",
            after: "intermediate changed\\n",
        },
    ];
    if coverage {
        fixtures.push(Fixture {
            name: "loom_actor",
            source: "crates/loom-actor/src/lib.rs",
            before: "",
            after: "",
        });
    }
    for fixture in fixtures {
        if std::env::var("LOOM_BENCH_CRATE").is_ok_and(|name| name != fixture.name) {
            continue;
        }
        let invocation: Invocation = serde_json::from_slice(
            &std::fs::read(root.join(format!("{}.json", fixture.name))).unwrap(),
        )
        .unwrap();
        let source = workspace.join(fixture.source);
        let original = std::fs::read_to_string(repo.join(fixture.source)).unwrap();
        if coverage {
            std::fs::write(&source, &original).unwrap();
            let output = root.join(format!("{}-coverage-output", fixture.name));
            std::fs::create_dir_all(&output).unwrap();
            let mut args = invocation.args.clone();
            let position = args.iter().position(|arg| arg == "--out-dir").unwrap();
            args[position + 1] = output.to_str().unwrap().into();
            let result = Command::new(&driver)
                .args(args)
                .current_dir(&invocation.cwd)
                .envs(&invocation.env)
                .env_remove("LOOM_OBJECT_CACHE")
                .env_remove("LOOM_ITEM_HASHES")
                .env_remove("LOOM_ITEM_PREIMAGES")
                .env(
                    "LOOM_ITEM_COVERAGE",
                    root.join(format!("{}-coverage.json", fixture.name)),
                )
                .output()
                .unwrap();
            std::fs::write(output.join("stderr.log"), &result.stderr).unwrap();
            let stderr = String::from_utf8_lossy(&result.stderr);
            assert!(result.status.success(), "{stderr}");
            for line in stderr.lines().filter(|line| {
                line.starts_with("item-coverage:") || line.starts_with("mono-coverage:")
            }) {
                println!("{} {line}", fixture.name);
            }
            continue;
        }
        assert_eq!(original.matches(fixture.before).count(), 1);
        let changed = original.replace(fixture.before, fixture.after);
        let mut cached = Vec::new();
        let mut uncached = Vec::new();
        for trial in 0..if smoke { 1 } else { 5 } {
            let trial_root = root.join(format!(
                "{}-{}-{trial}",
                fixture.name,
                if smoke { "smoke" } else { "measure" }
            ));
            let cache = trial_root.join("cache");
            if cache.exists() {
                std::fs::remove_dir_all(&cache).unwrap();
            }
            std::fs::write(&source, &original).unwrap();
            if smoke {
                let sample = compile(&driver, &invocation, &trial_root.join("native"), None);
                println!("native smoke {} {:.3}ms", fixture.name, sample.wall_ms);
            }
            let warm = compile(&driver, &invocation, &trial_root.join("warm"), Some(&cache));
            println!("warm {} {}", fixture.name, warm.stats);
            std::fs::write(&source, &changed).unwrap();
            if trial % 2 == 0 {
                cached.push(compile(
                    &driver,
                    &invocation,
                    &trial_root.join("cached"),
                    Some(&cache),
                ));
                uncached.push(compile(
                    &driver,
                    &invocation,
                    &trial_root.join("uncached"),
                    None,
                ));
            } else {
                uncached.push(compile(
                    &driver,
                    &invocation,
                    &trial_root.join("uncached"),
                    None,
                ));
                cached.push(compile(
                    &driver,
                    &invocation,
                    &trial_root.join("cached"),
                    Some(&cache),
                ));
            }
            println!(
                "trial {} {trial} {} cached_ms={:.3} uncached_ms={:.3}",
                fixture.name,
                cached.last().unwrap().stats,
                cached.last().unwrap().wall_ms,
                uncached.last().unwrap().wall_ms
            );
        }
        std::fs::write(&source, original).unwrap();
        assert!(
            cached.iter().all(|sample| sample.stats == cached[0].stats),
            "inconsistent cache counts"
        );
        let phase_text = cached[0]
            .phases
            .keys()
            .map(|name| {
                format!(
                    " {name}={:.3}",
                    median(cached.iter().map(|sample| sample.phases[name]).collect())
                )
            })
            .collect::<String>();
        println!(
            "object-cache real measurement: crate={} opt_level=2 trials={} {} median_with_cache_ms={:.3} median_without_cache_ms={:.3}{}",
            fixture.name,
            cached.len(),
            cached[0]
                .stats
                .strip_prefix("object-cache: ")
                .unwrap_or("stats_missing"),
            median(cached.iter().map(|s| s.wall_ms).collect()),
            median(uncached.iter().map(|s| s.wall_ms).collect()),
            phase_text
        );
    }
}
