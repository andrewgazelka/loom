# Definitions as files: version control and sharing through git

Status: built 2026-09-30 (v1). User request: "someone is adding a bunch of Rust definitions; they should be
version controlled in git with a file store of their own, so they can share with others via git". The CARv1 `export`/`import` bundle (`docs/bundles.md`) stays the binary transport;
this is the human and git form.

## Principle

Git holds **sources and a lock**, never the store. The store (SQLite, objects, caches) is a rebuildable
cache of what the directory says. A definition's hash comes from compiling its source with the pinned
toolchain, so the same files give the same hashes on every machine with the same toolchain; the lock file
records the hashes so a checkout can prove it got what the author had.

## Layout

```
defs/                     # any directory; one per project or per team
  loom.toml               # toolchain pin, registry settings, prefix to import under
  loom.lock               # generated: name -> hash, dependency pins, toolchain hash
  surface/                # one directory per definition (its name; nesting = `a/b` names)
    lib.rs                # the source, byte for byte what `add` would receive
    def.toml              # deps = { height = "terrain/height" }, allowed_effects, entry notes
    Cargo.toml            # optional: crates.io deps (with Cargo.lock, admission rules unchanged)
  terrain/
    height/lib.rs
```

Plain files, so `git diff`, code review, blame and merges work on the Rust itself. `loom.lock` is sorted
and one line per name, so it merges well and is always regenerated (never hand-resolved).

## Verbs (HTTP, CLI and MCP parity, like the existing definition verbs)

* `export_dir <dir> [names...]`: write the named definitions (default: all current names) as files, plus a
  fresh `loom.lock`. Sources come from the store byte for byte (`view.source`), so a round trip is exact.
* `import_dir <dir> [--into prefix]`: read the files, order them by dependencies, and add or update every
  name in one atomic step like `import`: a new name is `add`, a changed source is `update` (callers
  propagate as usual), an unchanged one is skipped. All or nothing; a dependency cycle or a missing
  dependency names the file. Afterwards every name's hash is compared with `loom.lock`: a difference is a
  warning that says which toolchain hash each side had (a different compiler legitimately changes hashes).
* `status <dir>`: per name, `same`, `changed` (source differs from the store), `new`, `removed`, and
  whether the lock matches; nothing is written.
* `watch <dir>` (later): `import_dir` on file changes, so an editor and the REPL share the working tree.

## Sharing

* Someone else clones the repo and runs `import_dir`; the names and hashes come out the same.
* Two people editing one name is an ordinary git conflict in `lib.rs`; resolve it there, re-run
  `import_dir`, commit the regenerated lock.
* A dependency from another repository is named in `def.toml` by git URL, revision and name; `import_dir`
  vendors nothing, it fetches that repository's directory at the revision and imports it under a prefix.
  Until that exists, use a submodule or subtree and import both directories.
* Names are the public interface; hashes are the pins. A dependency written `alias = "name"` is pinned to the
  name's current hash at import, as `add --dep` does, and `loom.lock` records the pin.

## Why not just commit CAR bundles

A CARv1 file is binary: no diffs, no review, every change rewrites the file. It remains right for moving a
closure with its identities and vendored crates between stores (`export`/`import`). The directory form is
the reviewable source of record; a bundle can always be made from it.

## Open questions

* Multi-file crates (modules in several files): `lib.rs` plus sibling files in the definition's directory,
  stored as the source bundle `add` already accepts for manifests.
* Whether `loom.lock` should also pin the toolchain's rustc hash per entry or once per file (once: simpler).
* Effect policy (`allowed_effects`) and entry-selection notes live in `def.toml`; the hash does not depend
  on them, so they are not in the lock.

## Built (v1) and how it differs from the design above

* The daemon speaks documents, not paths: `export_defs` and `import_defs` take and give
  `{name, source, deps, allowed_effects, manifest, lock}` (`crates/loom-api/src/defs_docs.rs`), so the server never
  reads or writes a client's files and an MCP agent or script can use the same verbs. The directory layout is
  `crates/loom-defdir`; `loom export-dir`, `loom import-dir` and `loom status-dir` (`crates/loom-cli/src/defdir.rs`)
  connect the two.
* `export_defs` needs the define scope (it returns each definition's manifest and lock, like `export`).
* `import_defs` is **not atomic across the batch** (each definition is its own `add` or `update`); it orders by
  dependency, continues past a failure, and skips everything that depends on a failed definition. It reports
  `added`, `updated`, `unchanged`, `failed` and `mismatches` (definitions whose hash differs from `loom.lock`).
* `loom.toml`, `watch`, dependencies on other git repositories and multi-file definitions are not built. A
  definition with files beyond `lib.rs`, `Cargo.toml` and `Cargo.lock` is refused by `export_defs`.
* `loom.lock` is TOML: the toolchain hash of the first exported definition and `name = hash` lines, sorted.
* Checked live (2026-09-30, this Mac): importing two definitions (`demo/height`, `demo/surface` depending on it)
  added both in dependency order; exporting them back gave byte-identical sources (comments kept); editing
  `height` made `status-dir` say `changed`, `import-dir` updated it (its dependent followed, as `update` always
  does) and reported both hashes as mismatches against the stale lock; a second import changed nothing; a
  definition that failed to build made its dependent `failed` without trying it.

### Review fixes (same day)

* A failed `update` is now a failed import: `update` answers `Ok` with a parked `needs_repair` session when it cannot
  rebuild, so `import_defs` reads the session status, aborts the session, reports `failed` with the compiler's
  message and skips the dependents. (It used to report `updated` with the old hash.)
* Dependencies are compared by what they resolve to (a name in the file and a hash in the store are equal when the
  name points at that hash), so a re-import converges. A dependency pinned to a hash no name points at is exported
  as that hash with a warning: it cannot be resolved on another daemon.
* A policy (`allowed_effects`) cannot be removed by an import (`update` keeps it); the document is refused rather
  than silently ignored. Labels are compared sorted and without repeats. `def.toml` and `loom.lock` reject unknown keys.
* `loom-defdir` reads and writes regular files only: symlinked `lib.rs`/`def.toml`/`Cargo.*`/`loom.lock` are refused on
  read, directories that are symlinks are refused on write, files are written by temp-and-rename (a link already
  there is replaced, never written through). All names are validated, and names that differ only by case are
  refused, before the first byte is written. Exporting a subset merges into `loom.lock` instead of replacing it.
* `import-dir` warns when `loom.lock` was written by a different compiler, and on hash mismatches; `status-dir`
  treats `removed` as information.
* Known limits: each changed definition is its own `update` (a chain of k edited definitions rebuilds about k²/2
  times); a whole directory is one request under the daemon's 16 MiB body limit; an import overwrites daemon-side
  edits (the old hash stays in `history`), because the lock records only the state after the author's edit.
