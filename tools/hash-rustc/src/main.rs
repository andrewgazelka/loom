#![feature(rustc_private)]

extern crate rustc_ast;
extern crate rustc_driver;
extern crate rustc_hir;
extern crate rustc_interface;
extern crate rustc_middle;
extern crate rustc_span;

mod encode;
mod entries;
mod graph;
mod preimages;

use std::path::PathBuf;
use std::process::{Command, ExitCode};

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
    }

    fn after_analysis<'tcx>(&mut self, _: &interface::Compiler, tcx: TyCtxt<'tcx>) -> Compilation {
        if self.destination.is_some() {
            self.document = Some(graph::collect(tcx));
        }
        Compilation::Continue
    }
}

fn main() -> ExitCode {
    rustc_driver::catch_with_exit_code(|| {
        let mut callbacks = HashCallbacks {
            destination: std::env::var_os("LOOM_ITEM_HASHES").map(PathBuf::from),
            document: None,
        };
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
        let preimages = std::env::var_os("LOOM_ITEM_PREIMAGES").map(PathBuf::from);
        if callbacks.destination.is_some() && preimages.is_none() {
            eprintln!("hash-rustc: LOOM_ITEM_HASHES requires LOOM_ITEM_PREIMAGES=<directory>");
            return ExitCode::FAILURE;
        }
        let args: Vec<String> = std::env::args().collect();
        rustc_driver::run_compiler(&args, &mut callbacks);
        if let Some(mut document) = callbacks.document {
            let compiler = PathBuf::from(env!("HASH_RUSTC_SYSROOT")).join("bin/rustc");
            let output = Command::new(compiler)
                .arg("-vV")
                .output()
                .expect("pinned rustc -vV");
            assert!(output.status.success(), "pinned rustc -vV failed");
            document.toolchain = String::from_utf8(output.stdout).expect("UTF-8 rustc version");
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
        }
        ExitCode::SUCCESS
    })
}
