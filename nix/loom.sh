#!@bash@
set -euo pipefail
export PATH='@runtimePath@':"${PATH:-}"
export SSL_CERT_FILE='@certificates@'
export NIX_SSL_CERT_FILE="$SSL_CERT_FILE"
export LOOM_ROOT='@sources@'
export RUSTC='@rustc@'
export LOOM_HASH_RUSTC='@hashRustc@'
if [[ ${1:-} == --help || ${1:-} == -h ]]; then
  exec '@daemon@' "$@"
fi
if [[ $(uname -s) == Darwin ]]; then
  data_home=${XDG_DATA_HOME:-"$HOME/Library/Application Support"}
  # rustc puts its sysroot's lib directory on the linker's dyld search path, so a
  # clang that loads libLLVM.dylib by name (nixpkgs' clang) resolves the guest
  # toolchain's libLLVM instead of its own and aborts on a missing symbol
  # (_LLVMInitializeLanaiAsmParser). Apple's cc links against no LLVM dylib.
  if [[ ! -x /usr/bin/cc ]]; then
    printf 'Loom needs /usr/bin/cc (Xcode Command Line Tools) to link guest build scripts on macOS\n' >&2
    exit 1
  fi
  export CARGO_TARGET_AARCH64_APPLE_DARWIN_LINKER=/usr/bin/cc
else
  data_home=${XDG_DATA_HOME:-"$HOME/.local/share"}
fi
state=${LOOM_DATA_DIR:-"$data_home/loom"}
umask 077
mkdir -p "$state"
export LOOM_BUILD_DIR=${LOOM_BUILD_DIR:-"$state/builds"}
export CARGO_HOME=${CARGO_HOME:-"$state/cargo"}
export CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS:-4}
mkdir -p "$LOOM_BUILD_DIR" "$CARGO_HOME"
if [[ ! -w "$LOOM_BUILD_DIR" || ! -w "$CARGO_HOME" ]]; then
  printf 'LOOM_BUILD_DIR and CARGO_HOME must be writable directories\n' >&2
  exit 1
fi
has_db=false
has_bind=false
has_auth=false
has_actors_dir=false
using_token_file=false
for argument in "$@"; do
  case "$argument" in
    --db|--db=*) has_db=true ;;
    --bind|--bind=*) has_bind=true ;;
    --actors-dir|--actors-dir=*) has_actors_dir=true ;;
    --token|--token=*|--tokens-file|--tokens-file=*) has_auth=true ;;
  esac
done
if $has_auth; then unset LOOM_TOKEN; fi
if ! $has_auth && [[ -z ${LOOM_TOKEN:-} ]]; then
  if [[ ! -s "$state/token" ]]; then
    token=$(od -An -N32 -tx1 /dev/urandom | tr -d ' \n')
    if ! (set -o noclobber; printf '%s\n' "$token" > "$state/token"); then
      if [[ ! -s "$state/token" ]]; then
        printf 'Cannot create token file: %s/token\n' "$state" >&2
        exit 1
      fi
    fi
  fi
  using_token_file=true
  LOOM_TOKEN=$(cat "$state/token")
  export LOOM_TOKEN
fi
bind=${LOOM_BIND:-127.0.0.1:8787}
defaults=()
if ! $has_db; then defaults+=(--db "$state/loom.sqlite"); fi
if ! $has_actors_dir; then defaults+=(--actors-dir "$state/actors"); fi
if ! $has_bind; then
  defaults+=(--bind "$bind")
  printf 'Loom: http://%s\n' "$bind" >&2
fi
if $using_token_file; then printf 'Token file: %s/token\n' "$state" >&2; fi
exec '@daemon@' "${defaults[@]}" "$@"
