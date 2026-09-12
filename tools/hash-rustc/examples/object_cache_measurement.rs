use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

const MODULES: usize = 48;

fn fixture(changed: bool) -> String {
    let mut source = String::new();
    for module in 0..MODULES {
        source.push_str(&format!(
            "pub mod module_{module} {{ #[inline(never)] pub fn value(mut x: u64) -> u64 {{\n"
        ));
        // A long dependency chain survives optimization, with a unique body per CGU.
        for step in 0..128 {
            let constant =
                17 + module * 257 + step * 3 + usize::from(changed && module == 0 && step == 0);
            source.push_str(&format!("x = (x << 3) ^ (x >> 5) ^ {constant};\n"));
        }
        source.push_str("x } }\n");
    }
    source
}

fn compile(
    driver: &Path,
    directory: &Path,
    source: &str,
    cache: Option<&Path>,
    expected_hits: usize,
) -> Duration {
    std::fs::create_dir_all(directory).unwrap();
    std::fs::write(directory.join("fixture.rs"), source).unwrap();
    let mut command = Command::new(driver);
    command
        .current_dir(directory)
        .args([
            "fixture.rs",
            "--crate-type=rlib",
            "--crate-name=measurement",
            "--edition=2024",
            "-C",
            "lto=off",
            "-C",
            "embed-bitcode=no",
            "-C",
            "debuginfo=0",
            "-C",
            "opt-level=2",
            "-C",
            &format!("codegen-units={MODULES}"),
        ])
        .env_remove("LOOM_ITEM_HASHES")
        .env_remove("LOOM_ITEM_PREIMAGES")
        .env_remove("LOOM_OBJECT_CACHE_FAIL_PUBLISH")
        .env_remove("LOOM_OBJECT_CACHE")
        .env_remove("LOOM_OBJECT_CACHE_STATS");
    if let Some(cache) = cache {
        command
            .env("LOOM_OBJECT_CACHE", cache)
            .env("LOOM_OBJECT_CACHE_STATS", "1");
    }
    let start = Instant::now();
    let output = command.output().unwrap();
    let duration = start.elapsed();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    if cache.is_some() {
        let line = stderr
            .lines()
            .find(|line| line.starts_with("object-cache: cgus="))
            .expect("cache stats");
        let values: std::collections::BTreeMap<_, _> = line
            .split_whitespace()
            .skip(1)
            .map(|field| field.split_once('=').unwrap())
            .collect();
        assert_eq!(
            values["cgus"].parse::<usize>().unwrap(),
            MODULES,
            "{stderr}"
        );
        assert_eq!(
            values["hits"].parse::<usize>().unwrap(),
            expected_hits,
            "{stderr}"
        );
        assert_eq!(
            values["misses"].parse::<usize>().unwrap(),
            MODULES - expected_hits,
            "{stderr}"
        );
        println!("{line}");
    }
    duration
}

fn main() {
    let mut arguments = std::env::args_os().skip(1);
    let driver = std::fs::canonicalize(
        arguments
            .next()
            .expect("usage: object_cache_measurement <hash-rustc-path>"),
    )
    .expect("driver path must exist");
    assert!(
        arguments.next().is_none(),
        "usage: object_cache_measurement <hash-rustc-path>"
    );
    let root = tempfile::tempdir().unwrap();
    let base = fixture(false);
    let changed = fixture(true);
    let mut cached = Vec::new();
    let mut uncached = Vec::new();
    for trial in 0..5 {
        let directory = root.path().join(format!("trial-{trial}"));
        let cache = directory.join("cache");
        compile(&driver, &directory.join("warm"), &base, Some(&cache), 0);
        // Alternate order to reduce systematic temperature/order bias.
        if trial % 2 == 0 {
            cached.push(compile(
                &driver,
                &directory.join("cached"),
                &changed,
                Some(&cache),
                MODULES - 1,
            ));
            uncached.push(compile(
                &driver,
                &directory.join("uncached"),
                &changed,
                None,
                0,
            ));
        } else {
            uncached.push(compile(
                &driver,
                &directory.join("uncached"),
                &changed,
                None,
                0,
            ));
            cached.push(compile(
                &driver,
                &directory.join("cached"),
                &changed,
                Some(&cache),
                MODULES - 1,
            ));
        }
    }
    cached.sort();
    uncached.sort();
    println!(
        "object-cache measurement: fixture_modules={MODULES} trials=5 median_with_cache_ms={:.3} median_without_cache_ms={:.3}",
        cached[2].as_secs_f64() * 1000.0,
        uncached[2].as_secs_f64() * 1000.0
    );
}
