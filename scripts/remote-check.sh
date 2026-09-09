#!/usr/bin/env bash
set -euo pipefail

# One Linux integration lane; the target cache survives source snapshots.
session=01a084e8-9989-7d50-87aa-0e56e996314f
if [[ ${1:-} == --stage ]]; then
  export PATH=/run/current-system/sw/bin:$PATH
  base=$2
  while IFS= read -r -d '' path; do
    previous="$base/source/${path#"$base/staging/"}"
    if [[ -f "$previous" ]] && cmp -s "$path" "$previous"; then
      touch -r "$previous" "$path"
    else
      touch "$path"
    fi
  done < <(find "$base/staging" -type f -print0)
  # Only trusted dependency compilation is shared; component artifacts and
  # untrusted per-closure sandbox targets stay in the replaced source tree.
  if [[ ! -e "$base/guest-target" && -d "$base/source/.loom-build/rust-target" ]]; then
    mv "$base/source/.loom-build/rust-target" "$base/guest-target"
  fi
  mkdir -p "$base/guest-target" "$base/staging/.loom-build"
  ln -s "$base/guest-target" "$base/staging/.loom-build/rust-target"
  [[ ! -d "$base/source" ]] || mv "$base/source" "$base/previous"
  mv "$base/staging" "$base/source"
  rm -rf "$base/previous"
  exit 0
fi
if [[ ${1:-} == --worker ]]; then
  export PATH=/run/current-system/sw/bin:$PATH
  base=$2
  action=$3
  exec 9>"$base/build.lock"
  flock -n 9 || { echo 'Another loom verification owns the Linux lane'; exit 75; }
  exec > >(tee "$base/$action.log") 2>&1
  rustc_root=/nix/store/l3gishxdaxhnaxczyw0jw7dcy9p4rhcs-rustc-1.97.0
  cargo_root=/nix/store/l7v00m55rb130bhql2kr74pq8sdpl41c-cargo-1.97.0
  cc_root=/nix/store/adcz0m6qq2flmshdf0zz2xwjr5zbq1gr-gcc-wrapper-15.3.0
  for root in "$rustc_root" "$cargo_root" "$cc_root"; do nix path-info "$root" >/dev/null; done
  export PATH="$rustc_root/bin:$cargo_root/bin:$cc_root/bin:$PATH"
  for tool in "$base"/tools*; do
    [[ ! -d "$tool/bin" ]] || export PATH="$tool/bin:$PATH"
  done
  export CARGO_TARGET_DIR="$base/target" CARGO_BUILD_JOBS=8
  export CARGO_HOME="$base/cargo"
  if [[ -x "$base/bin/rustc" ]]; then
    export RUSTC="$base/bin/rustc" RUSTDOC="$base/official/bin/rustdoc"
    export PATH="$base/official/bin:$PATH"
  fi
  cd "$base/source"
  rustc --version
  cargo --version
  uptime
  status=0
  case "$action" in
    prepare) ;;
    tools) nix build --max-jobs 1 --cores 8 --out-link "$base/tools" nixpkgs#cargo-component nixpkgs#binaryen nixpkgs#podman nixpkgs#bubblewrap || status=$? ;;
    machine)
      nix build --max-jobs 1 --cores 8 --out-link "$base/static-busybox" nixpkgs#pkgsStatic.busybox
      LOOM_STATIC_BUSYBOX="$base/static-busybox/bin/busybox" cargo test --locked -p loom-rt machine::tests::hermetic_exec_uses_snapshot_and_caches_across_actors -- --ignored --exact || status=$?
      ;;
    targets)
      mkdir -p "$base/official" "$base/downloads" "$base/bin"
      for package in rustc-1.97.0-x86_64-unknown-linux-gnu rust-std-1.97.0-x86_64-unknown-linux-gnu rust-std-1.97.0-wasm32-wasip1 rust-std-1.97.0-wasm32-wasip2; do
        archive="$package.tar.xz"
        cd "$base/downloads"
        [[ -f "$archive" ]] || curl --fail --location --retry 2 -o "$archive" "https://static.rust-lang.org/dist/$archive"
        curl --fail --location --retry 2 -o "$archive.sha256" "https://static.rust-lang.org/dist/$archive.sha256"
        sha256sum -c "$archive.sha256"
        tar -xf "$archive"
        bash "$package/install.sh" --prefix="$base/official" --disable-ldconfig
      done
      printf '#!/run/current-system/sw/bin/bash\nexec %q "$@"\n' "$base/official/bin/rustc" > "$base/bin/rustc.new"
      chmod +x "$base/bin/rustc.new"
      mv "$base/bin/rustc.new" "$base/bin/rustc"
      "$base/bin/rustc" --version
      printf 'fn main() { println!("loom WASI toolchain smoke"); }\n' > "$base/downloads/smoke.rs"
      "$base/bin/rustc" --target wasm32-wasip2 --emit=obj "$base/downloads/smoke.rs" -o "$base/downloads/smoke.o"
      ;;
    check) cargo check --workspace --all-targets --locked || status=$? ;;
    test) cargo test --workspace --locked || status=$? ;;
    fetch) cargo fetch --locked || status=$? ;;
    e2e) bash scripts/native-e2e.sh || status=$? ;;
    e2e_mcp) LOOM_E2E_SUITE=mcp bash scripts/native-e2e.sh || status=$? ;;
    e2e_languages) LOOM_E2E_SUITE=languages bash scripts/native-e2e.sh || status=$? ;;
    container|container_verify)
      [[ "$action" != container_verify ]] || export LOOM_SKIP_CONTAINER_BUILD=1 LOOM_CONTAINER_SUITE=vendor
      export LOOM_IMAGE_POLICY="$base/container-policy.json"
      cat > "$LOOM_IMAGE_POLICY" <<'POLICY'
{"default":[{"type":"reject"}],"transports":{"docker":{"docker.io/library/rust":[{"type":"insecureAcceptAnything"}],"docker.io/oven/bun":[{"type":"insecureAcceptAnything"}]},"containers-storage":{"":[{"type":"insecureAcceptAnything"}]}}}
POLICY
      bash scripts/container-smoke.sh || status=$?
      ;;
    vendor)
      cargo run --locked -p loom-build --example build_smoke -- "$PWD" rust examples/bundles/itoa.json "$base/itoa.wasm" || status=$?
      [[ "$status" -ne 0 ]] || test -s "$base/itoa.wasm" || status=$?
      ;;
    compiler)
      podman run --rm --memory=1g --cpus=1 --security-opt 'unmask=/proc/*' --entrypoint bash -i localhost/loom:local <<'COMPILER' || status=$?
set -euo pipefail
compiler=$(realpath "$(command -v cc)")
bwrap --unshare-all --clearenv --proc /proc --dev /dev --tmpfs /tmp \
  --ro-bind /usr /usr --ro-bind /bin /bin --ro-bind /lib /lib --ro-bind /lib64 /lib64 \
  --dir /opt/loom-bin --symlink "$compiler" /opt/loom-bin/cc \
  --setenv PATH /opt/loom-bin:/usr/bin:/bin /bin/sh -c \
  'printf "int main(void) { return 0; }\n" | cc -x c -o /tmp/compiler-witness - && /tmp/compiler-witness'
printf '1/1 canonical compiler link/run in nested sandbox passed\n'
COMPILER
      ;;
    vendor_run) cargo run --locked -p loom-rt --example component_smoke -- "$base/itoa.wasm" rust itoa || status=$? ;;
    sandbox) bash loom-rustc/test-sandbox.sh || status=$? ;;
    acceptance)
      export LOOM_STATIC_BUSYBOX="$base/static-busybox/bin/busybox"
      bash scripts/acceptance.sh || status=$?
      ;;
    m9)
      export LOOM_STATIC_BUSYBOX="$base/static-busybox/bin/busybox"
      (cd loom-checker && bun install --frozen-lockfile)
      (cd loom-guest-ts && bun install --frozen-lockfile)
      bash scripts/milestones/m9.sh || status=$?
      ;;
    *) echo "Unknown action: $action"; status=64 ;;
  esac
  printf 'LOOM_REMOTE_EXIT=%s\n' "$status"
  exit "$status"
fi

action=${1:-check}
case "$action" in prepare|check|test|fetch|tools|targets|machine|e2e|e2e_mcp|e2e_languages|sandbox|acceptance|container|container_verify|vendor|vendor_run|m9|compiler) ;; *) echo 'Usage: scripts/remote-check.sh [prepare|check|test|fetch|tools|targets|machine|e2e|e2e_mcp|e2e_languages|sandbox|acceptance|container|container_verify|vendor|vendor_run|m9|compiler]' >&2; exit 64;; esac
cd "$(dirname "$0")/.."
bash -n scripts/remote-check.sh
host=dev-compute-4
base="/home/andrew/loom-$session"
unit="loom-check-$session"
ssh "$host" "mkdir -p '$base'; if systemctl --user is-active --quiet '$unit'; then echo 'Linux verification already running' >&2; exit 75; fi; mkdir '$base/staging'"
inputs=(Cargo.toml crates loom-wit scripts)
for input in Cargo.lock Dockerfile .dockerignore deploy loom-checker loom-rustc loom-guest-ts loom-ui examples; do
  [[ ! -e "$input" ]] || inputs+=("$input")
done
tar_flags=()
[[ $(uname -s) != Darwin ]] || tar_flags+=(--no-xattrs --no-mac-metadata)
COPYFILE_DISABLE=1 tar "${tar_flags[@]}" --exclude=target --exclude=node_modules --exclude=.git --exclude=.DS_Store -czf - "${inputs[@]}" |
  ssh "$host" "tar -xzf - -C '$base/staging'; chmod +x '$base/staging/scripts/remote-check.sh'; /run/current-system/sw/bin/bash '$base/staging/scripts/remote-check.sh' --stage '$base'"
ssh "$host" "printf '%s\\t%s\\t%s\\t%s\\t%s\\n' '$session' '$unit' '8 cores' '24G' '$base' >> '$base/host-ledger.tsv'; systemd-run --user --unit='$unit' --wait --pipe --collect -p MemoryMax=24G -p CPUQuota=800% -p TasksMax=512 /run/current-system/sw/bin/bash '$base/source/scripts/remote-check.sh' --worker '$base' '$action'"
