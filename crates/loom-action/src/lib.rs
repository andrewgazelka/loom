//! Hermetic actions: a process run as a pure function of a declared input tree.
//!
//! An [`Action`] names a tool, its arguments and environment, the input files (by content
//! hash, from the store) and the output files it must produce. Its key is a hash of exactly
//! those things plus the tool binary's own content, so the same action is the same key on
//! any run of this machine. The [`Runner`] answers a repeated key from the store without
//! running anything, and otherwise runs the tool confined by `loom-process`'s sandbox: the
//! tool sees a scratch directory holding only the declared inputs, its own read-only runtime
//! closure, no network unless the action asks, and the environment the action lists and
//! nothing else. Anything else it reads fails closed. Only a run that exits 0 and produces
//! every declared output is recorded; a failure is returned but never cached.
//!
//! This is local first: identity is the tool's content hash (plus a caller string such as a
//! version banner), not a toolchain closure, so a result is valid on this machine's runtime
//! libraries and not portable across machines.
mod key;
mod materialize;
mod runner;

pub use key::{Action, Input};
pub use materialize::ingest_directory;
pub use runner::{ActionResult, Outcome, OutputFile, ROOT_TOKEN, Runner, Stats};
