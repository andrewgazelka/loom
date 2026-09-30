#![feature(rustc_private)]

extern crate rustc_ast;
extern crate rustc_codegen_llvm;
extern crate rustc_codegen_ssa;
extern crate rustc_data_structures;
extern crate rustc_driver;
extern crate rustc_errors;
extern crate rustc_hir;
extern crate rustc_hir_analysis;
extern crate rustc_interface;
extern crate rustc_metadata;
extern crate rustc_middle;
extern crate rustc_session;
extern crate rustc_span;
extern crate rustc_structures;
extern crate rustc_target;

mod cache_backend;
mod cache_flags;
mod cache_metrics;
mod coverage;
mod dependencies;
mod effects;
mod encode;
mod entries;
mod graph;
mod mono;
mod object_cache;
mod object_store;
mod preimages;
mod serve;

use std::path::PathBuf;
use std::process::ExitCode;

use rustc_driver::{Callbacks, Compilation};
use rustc_interface::interface;
use rustc_middle::ty::TyCtxt;

struct HashCallbacks {
    destination: Option<PathBuf>,
    document: Option<graph::Document>,
}

impl Callbacks for HashCallbacks {
    fn config(&mut self, config: &mut interface::Config) {
        // An installed rustc finds its sysroot relative to its executable. This
        // driver lives elsewhere; explicit --sysroot always takes precedence.
        config.opts.sysroot.default = PathBuf::from(env!("HASH_RUSTC_SYSROOT"));
        config.opts.unstable_opts.always_encode_mir = true;
        if std::env::var_os("LOOM_OBJECT_CACHE").is_some() {
            config.make_codegen_backend =
                Some(Box::new(|_| Box::new(cache_backend::CachingBackend::new())));
        }
    }

    fn after_analysis<'tcx>(&mut self, _: &interface::Compiler, tcx: TyCtxt<'tcx>) -> Compilation {
        if let Some(path) = std::env::var_os("LOOM_ITEM_COVERAGE") {
            coverage::write(tcx, &PathBuf::from(path));
        }
        if self.destination.is_some() {
            let started = std::time::Instant::now();
            let mut document = graph::collect(tcx);
            let collected = started.elapsed();
            document.effects = effects::collect(tcx);
            let analyzed = started.elapsed();
            document.schema = effects::schema(tcx);
            self.document = Some(document);
            timing(format_args!(
                "identity graph {} ms, effects {} ms, schema {} ms",
                collected.as_millis(),
                (analyzed - collected).as_millis(),
                (started.elapsed() - analyzed).as_millis()
            ));
        }
        Compilation::Continue
    }
}

/// `LOOM_DRIVER_TIMING=1` at driver (or `--loom-serve`) start prints where a
/// compile's time went to its standard error.
fn timing(message: std::fmt::Arguments<'_>) {
    if timing_enabled() {
        eprintln!("driver-timing: {message}");
    }
}

/// Read once, before a server replaces the process environment per request.
pub(crate) fn timing_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("LOOM_DRIVER_TIMING").is_some())
}

fn main() -> ExitCode {
    let mut arguments = std::env::args();
    if arguments.nth(1).as_deref() == Some("--loom-serve") {
        return serve::serve();
    }
    compile(std::env::args().collect())
}

/// One compile: `args` is a rustc command line, `args[0]` the program. The
/// process environment names the side outputs (`LOOM_ITEM_HASHES`, ...).
fn compile(args: Vec<String>) -> ExitCode {
    let result = rustc_driver::catch_with_exit_code(|| {
        let mut callbacks = HashCallbacks {
            destination: std::env::var_os("LOOM_ITEM_HASHES").map(PathBuf::from),
            document: None,
        };
        let preimages = std::env::var_os("LOOM_ITEM_PREIMAGES").map(PathBuf::from);
        if callbacks.destination.is_some() && preimages.is_none() {
            eprintln!("hash-rustc: LOOM_ITEM_HASHES requires LOOM_ITEM_PREIMAGES=<directory>");
            return ExitCode::FAILURE;
        }
        if let Some(path) = &callbacks.destination
            && let Err(error) = std::fs::remove_file(path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            eprintln!(
                "hash-rustc: cannot remove stale {}: {error}",
                path.display()
            );
            return ExitCode::FAILURE;
        }
        let started = std::time::Instant::now();
        rustc_driver::run_compiler(&args, &mut callbacks);
        timing(format_args!("run_compiler {} ms", started.elapsed().as_millis()));
        let started = std::time::Instant::now();
        if let Some(mut document) = callbacks.document {
            // `rustc -vV` of the compiler this driver links, captured by build.rs;
            // spawning it per compile cost a whole rustc start-up (about 45 ms).
            document.toolchain = include_str!(env!("HASH_RUSTC_VERSION_FILE")).to_owned();
            let path = callbacks.destination.expect("requested side output");
            if let Err(error) = document
                .preimages
                .write(preimages.as_deref().expect("preimage directory"))
            {
                eprintln!("hash-rustc: {error}");
                return ExitCode::FAILURE;
            }
            let result = serde_json::to_vec_pretty(&document)
                .map_err(std::io::Error::other)
                .and_then(|bytes| std::fs::write(&path, bytes));
            if let Err(error) = result {
                eprintln!("hash-rustc: cannot write {}: {error}", path.display());
                return ExitCode::FAILURE;
            }
            timing(format_args!("write documents {} ms", started.elapsed().as_millis()));
        }
        ExitCode::SUCCESS
    });
    object_cache::print_stats();
    cache_metrics::print();
    result
}
