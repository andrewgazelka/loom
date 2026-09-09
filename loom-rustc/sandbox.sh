#!/usr/bin/env bash
# Linux-only build boundary. Source roots must contain only materialized CAS input.
# Toolchains are public, immutable inputs; no caller home/config/credentials enter.
set -euo pipefail
if [[ $# != 5 || ( $1 != vendor && $1 != build ) ]]; then
  echo 'usage: sandbox.sh vendor|build SOURCE_ROOT CRATE_DIR TARGET_DIR REPO_ROOT' >&2
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
for command in bwrap timeout prlimit cargo rustc cc; do
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
# Rustup installs are exposed as toolchains only, never as an entire caller home.
rustup_root=${RUSTUP_HOME:-${HOME}/.rustup}
if [[ -d $rustup_root/toolchains ]]; then
  args+=(--ro-bind "$rustup_root/toolchains" /opt/rustup/toolchains)
  active=$(rustup show active-toolchain | cut -d ' ' -f 1)
  [[ -d $rustup_root/toolchains/$active/bin ]] || { echo 'active toolchain absent' >&2; exit 69; }
  args+=(--setenv PATH "/opt/rustup/toolchains/$active/bin:/opt/loom-bin:$cc_dir:/run/current-system/sw/bin:/usr/local/bin:/usr/bin:/bin")
else
  # An administrator-installed standalone Rust distribution needs its sysroot.
  rust_bin=$(realpath "$(command -v rustc)")
  rust_root=$(dirname "$(dirname "$rust_bin")")
  args+=(--ro-bind "$rust_root" /opt/loom-rust --setenv RUSTC /opt/loom-rust/bin/rustc)
  args+=(--setenv PATH "/opt/loom-rust/bin:$(dirname "$(command -v cargo)"):/opt/loom-bin:$cc_dir:/run/current-system/sw/bin:/usr/local/bin:/usr/bin:/bin")
fi
args+=(--dir /opt/loom-bin)
# Debian's cc symlink crosses /etc/alternatives, which is deliberately absent.
# Link to its canonical public path so GCC still finds its installed plugins.
# Nix wrappers retain their original invocation path and adjacent nix-support.
cc_executable=$(realpath "$(command -v cc)")
case "$cc_executable" in
  /nix/store/*) ;;
  *) args+=(--symlink "$cc_executable" /opt/loom-bin/cc) ;;
esac
if command -v cargo-component >/dev/null; then
  args+=(--ro-bind "$(realpath "$(command -v cargo-component)")" /opt/loom-bin/cargo-component)
fi
args+=(--ro-bind "$source_root" "$source_root" --bind "$target_dir" "$target_dir")
for guest in loom-guest-rs loom-guest-macros loom-proto; do
  args+=(--ro-bind "$repo_root/crates/$guest" "$repo_root/crates/$guest")
done
args+=(--ro-bind "$repo_root/loom-wit" "$repo_root/loom-wit"
  --ro-bind "$repo_root/loom-rustc/build.sh" /opt/build.sh --chdir "$crate_dir")
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
  run=(/bin/sh -eu -c 'if [ -f Cargo.lock ]; then cargo vendor --locked vendor; else cargo vendor vendor; fi > .cargo/config.toml')
else
  [[ -f $crate_dir/Cargo.lock && -f $crate_dir/.cargo/config.toml && -d $crate_dir/vendor ]] || {
    echo 'offline build requires locked, vendored sources' >&2; exit 65;
  }
  expected_config=$(cat "$repo_root/loom-rustc/vendor-config.toml")
  [[ ! -e $crate_dir/.cargo/config && $(cat "$crate_dir/.cargo/config.toml") == "$expected_config" ]] || {
    echo 'offline Cargo configuration differs from the fixed vendor contract' >&2; exit 65;
  }
  args+=(--setenv CARGO_NET_OFFLINE true --setenv LOOM_LOCKED 1)
  run=(/bin/sh /opt/build.sh "$crate_dir" "$target_dir")
fi
# Per-process limits supplement the worker cgroup (MemoryMax/CPUQuota/TasksMax).
# timeout ends the namespace; bwrap kills sandbox descendants on parent exit.
exec timeout --signal=TERM --kill-after=5s 300s \
  prlimit --as=8589934592 --cpu=300 --nproc=256 --nofile=1024 -- \
  bwrap "${args[@]}" "${run[@]}"
