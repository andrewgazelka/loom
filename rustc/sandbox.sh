#!/usr/bin/env bash
# Linux-only build boundary. Source roots must contain only materialized CAS input.
# Toolchains are public, immutable inputs; no caller home/config/credentials enter.
set -euo pipefail
if [[ $# != 5 || ( $1 != vendor && $1 != build && $1 != rustc && $1 != metadata ) ]]; then
  echo 'usage: sandbox.sh vendor|build|rustc|metadata SOURCE_ROOT CRATE_DIR TARGET_DIR REPO_ROOT' >&2
  exit 64
fi
mode=$1
source_root=$(realpath "$2")
crate_dir=$(realpath "$3")
mkdir -p "$4"
target_dir=$(realpath "$4")
repo_root=$(realpath "$5")
[[ $(uname -s) == Linux ]] || { echo 'Rust sandbox requires Linux' >&2; exit 69; }
case "$crate_dir/" in "$source_root/"*) ;; *) echo 'crate must be inside source root' >&2; exit 64;; esac
for command in bwrap timeout prlimit cargo cc; do
  command -v "$command" >/dev/null || { echo "missing sandbox tool: $command" >&2; exit 69; }
done
# Clear the environment before Cargo, including RUSTC_WRAPPER and cargo config.
args=(--die-with-parent --new-session --unshare-all --clearenv --proc /proc --dev /dev --tmpfs /tmp
  --dir /home/guest --setenv HOME /home/guest --setenv CARGO_HOME /home/guest/.cargo
  --setenv CARGO_TARGET_DIR "$target_dir" --setenv CARGO_BUILD_JOBS 4
  --setenv PATH /usr/local/bin:/usr/bin:/bin --setenv LANG C.UTF-8)
for public in /nix/store /usr /bin /lib /lib64 /run/current-system/sw; do
  [[ ! -e $public ]] || args+=(--ro-bind "$public" "$public")
done
cc_dir=$(dirname "$(command -v cc)")
# Compilation requires the same explicit driver selected by the native caller.
# Resolution and sysroot discovery happen before clearing rustup's environment.
if [[ $mode == build || $mode == rustc ]]; then
  [[ -n ${RUSTC:-} ]] || { echo 'sandbox compilation requires RUSTC content-hashing driver' >&2; exit 69; }
fi
compiler_executable=$(command -v "${RUSTC:-rustc}")
[[ -x $compiler_executable ]] || { echo 'selected compiler executable is missing' >&2; exit 69; }
rust_root=$("$compiler_executable" --print sysroot)
if [[ $(basename "$(realpath "$compiler_executable")") == rustup ]]; then
  compiler_executable="$rust_root/bin/rustc"
fi
compiler_executable=$(realpath "$compiler_executable")
[[ -x $rust_root/bin/rustc ]] || { echo 'selected Rust sysroot is incomplete' >&2; exit 69; }
cargo_executable=$(command -v cargo)
# Rustup's proxy needs the caller home; use its selected real Cargo instead.
if [[ $(basename "$(realpath "$cargo_executable")") == rustup ]]; then
  cargo_executable="$rust_root/bin/cargo"
fi
[[ -x $cargo_executable ]] || { echo 'selected Cargo executable is missing' >&2; exit 69; }
args+=(--ro-bind "$cargo_executable" "$cargo_executable")
cargo_dir=$(dirname "$cargo_executable")
args+=(--setenv RUSTC "$compiler_executable")
sandbox_path="$rust_root/bin:$cargo_dir:/opt/loom-bin:$cc_dir:/run/current-system/sw/bin:/usr/local/bin:/usr/bin:/bin"
# Packaged helper tools live in separate Nix outputs. Retain public immutable
# directories, while dropping caller-local executable/configuration directories.
IFS=: read -r -a host_path <<< "$PATH"
for directory in "${host_path[@]}"; do
  case "$directory" in
    /nix/store/*/bin) sandbox_path="$sandbox_path:$directory" ;;
  esac
done
args+=(--setenv PATH "$sandbox_path")
args+=(--dir /opt/loom-bin)
# Debian's cc symlink crosses /etc/alternatives, which is deliberately absent.
# Link to its canonical public path so GCC still finds its installed plugins.
# Nix wrappers retain their original invocation path and adjacent nix-support.
cc_executable=$(realpath "$(command -v cc)")
case "$cc_executable" in
  /nix/store/*) ;;
  *) args+=(--symlink "$cc_executable" /opt/loom-bin/cc) ;;
esac
args+=(--ro-bind "$source_root" "$source_root" --bind "$target_dir" "$target_dir")
# Mount after the source/target trees so an overlapping cache cannot hide or
# make the driver writable. Its embedded sysroot and loader rpath stay valid.
args+=(--ro-bind "$rust_root" "$rust_root"
  --ro-bind "$compiler_executable" "$compiler_executable")
if [[ $mode == rustc && -n ${LOOM_ITEM_HASHES:-} ]]; then
  args+=(--setenv LOOM_ITEM_HASHES "$LOOM_ITEM_HASHES"
    --setenv LOOM_ITEM_PREIMAGES "${LOOM_ITEM_PREIMAGES:?missing item preimages directory}")
fi
for guest in loom-guest-rs loom-proto; do
  args+=(--ro-bind "$repo_root/crates/$guest" "$repo_root/crates/$guest")
done
args+=(--ro-bind "$repo_root/rustc/build.sh" /opt/build.sh
  --ro-bind "$repo_root/rustc/capture.sh" /opt/capture.sh --chdir "$crate_dir")
if [[ $mode == vendor ]]; then
  # Cargo vendor resolves/downloads packages but never executes their build scripts.
  # This is the sole network-capable phase; it has no application secrets.
  [[ ! -e $crate_dir/.cargo ]] || { echo 'caller Cargo configuration is forbidden' >&2; exit 65; }
  mkdir -p "$crate_dir/.cargo"
  args+=(--share-net --bind "$crate_dir" "$crate_dir")
  for public in /etc/resolv.conf /etc/hosts /etc/ssl/certs /etc/pki; do
    [[ ! -e $public ]] || args+=(--ro-bind "$public" "$public")
  done
  ca_bundle=${NIX_SSL_CERT_FILE:-${SSL_CERT_FILE:-/etc/ssl/certs/ca-certificates.crt}}
  [[ -f $ca_bundle ]] || { echo 'trusted CA bundle unavailable' >&2; exit 69; }
  args+=(--ro-bind "$(realpath "$ca_bundle")" /opt/ca-bundle.crt
    --setenv CARGO_HTTP_CAINFO /opt/ca-bundle.crt --setenv SSL_CERT_FILE /opt/ca-bundle.crt)
  library_manifest="$rust_root/lib/rustlib/src/rust/library/Cargo.toml"
  [[ -f $library_manifest ]] || { echo 'shared core builds require matching rust-src' >&2; exit 69; }
  args+=(--setenv RUSTC_BOOTSTRAP 1 --setenv LOOM_RUST_SRC_MANIFEST "$library_manifest")
  # The guest shell expands these variables after bwrap installs its environment.
  # shellcheck disable=SC2016
  run=(/bin/sh -eu -c '
    cargo metadata --format-version=1 > /dev/null
    cargo vendor --locked --sync "$LOOM_RUST_SRC_MANIFEST" vendor > .cargo/config.toml
    # Preserve original archives for offline compiler-lock source verification.
    # These bytes are checked against the selected compiler lock before trust.
    for archive in "$CARGO_HOME"/registry/cache/*/*.crate; do
      [ -f "$archive" ] || continue
      cp "$archive" "vendor/.loom-archive-${archive##*/}"
    done
  ')

else
  [[ -f $crate_dir/Cargo.lock && -f $crate_dir/.cargo/config.toml && -d $crate_dir/vendor ]] || {
    echo 'offline build requires locked, vendored sources' >&2; exit 65;
  }
  expected_config=$(cat "$repo_root/rustc/vendor-config.toml")
  [[ ! -e $crate_dir/.cargo/config && $(cat "$crate_dir/.cargo/config.toml") == "$expected_config" ]] || {
    echo 'offline Cargo configuration differs from the fixed vendor contract' >&2; exit 65;
  }
  args+=(--setenv CARGO_NET_OFFLINE true --setenv LOOM_LOCKED 1
    --setenv LOOM_RUST_TARGET "${LOOM_RUST_TARGET:-wasm32-unknown-unknown}"
    --setenv LOOM_CAS_SOURCES "$source_root/source-trees")
  if [[ $mode == build && -n ${LOOM_COMPILER_CACHE_OWNER:-} ]]; then
    args+=(--ro-bind "$LOOM_COMPILER_CACHE_OWNER" "$LOOM_COMPILER_CACHE_OWNER"
      --setenv LOOM_COMPILER_CACHE_OWNER "$LOOM_COMPILER_CACHE_OWNER"
      --setenv LOOM_COMPILER_CACHE_MIRROR "$LOOM_COMPILER_CACHE_MIRROR"
      --setenv LOOM_ROOT_INCREMENTAL "$LOOM_ROOT_INCREMENTAL"
      --setenv LOOM_TRUSTED_SOURCES "$LOOM_TRUSTED_SOURCES")
  fi
  run=(/bin/sh /opt/build.sh "$crate_dir" "$target_dir")
  if [[ $mode == rustc ]]; then
    run=(/bin/sh "$target_dir/direct.sh")
  elif [[ $mode == metadata ]]; then
    run=(cargo metadata --locked --offline --format-version=1)
  fi
fi
# Per-process limits supplement the worker cgroup (MemoryMax/CPUQuota/TasksMax).
# timeout ends the namespace; bwrap kills sandbox descendants on parent exit.
exec timeout --signal=TERM --kill-after=5s 300s \
  prlimit --as=8589934592 --cpu=300 --nproc=256 --nofile=1024 -- \
  bwrap "${args[@]}" "${run[@]}"
