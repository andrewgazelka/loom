#!/bin/sh
# The caller clears ambient variables before Cargo. Preserve Cargo's complete
# rustc environment, including arbitrary cargo:rustc-env build-script outputs.
set -eu
# macOS removes DYLD_* at the /bin/sh boundary. Reestablish the compiler's own
# loader directory before invoking its native binary (and its rust-lld child).
if [ "$(uname -s)" = Darwin ]; then
  export DYLD_LIBRARY_PATH="${LOOM_COMPILER_LIB:?missing compiler library directory}"
fi
compile=no
for argument in "$@"; do
  case "$argument" in --print|--print=*|-) exec "$@" ;; esac
  if [ "$argument" = --error-format=json ]; then compile=yes; fi
done
if [ -z "${CARGO_MANIFEST_DIR:-}" ]; then exec "$@"; fi
if [ "$compile" = no ]; then exec "$@"; fi
# Every user definition, including definition dependencies, gets a correctness
# lint. The isolated Wasm runtime remains the execution boundary.
case "${CARGO_PRIMARY_PACKAGE:-}:${CARGO_PKG_NAME:-}" in
  1:*|*:loom-definition-*)
    # A dependency's Cargo cap-lints=allow otherwise downgrades even forbid.
    remaining=$#
    while [ "$remaining" -gt 0 ]; do
      argument=$1
      shift
      remaining=$((remaining - 1))
      case "$argument" in
        --cap-lints) shift; remaining=$((remaining - 1)) ;;
        --cap-lints=*) ;;
        *) set -- "$@" "$argument" ;;
      esac
    done
    set -- "$@" -Funsafe-code
    ;;
  *)
    # A content-addressed external source is still an external dependency. Give
    # it Cargo's registry lint cap, rather than workspace path lint semantics.
    if [ -n "${LOOM_CAS_SOURCES:-}" ]; then
      case "$CARGO_MANIFEST_DIR/" in "$LOOM_CAS_SOURCES/"*|*/loom-crates/*) set -- "$@" --cap-lints allow ;; esac
    fi
    ;;
esac
# rustc randomizes archive object-member names when incremental is enabled.
# Dependency artifacts must reproduce byte-for-byte; only the root keeps its
# incremental workspace for edits to that definition.
if [ "${CARGO_PRIMARY_PACKAGE:-}" != 1 ] || [ -n "${LOOM_ROOT_INCREMENTAL:-}" ]; then
  remaining=$#
  while [ "$remaining" -gt 0 ]; do
    argument=$1
    shift
    remaining=$((remaining - 1))
    case "$argument" in
      -C)
        option=$1
        shift
        remaining=$((remaining - 1))
        case "$option" in incremental=*) ;; *) set -- "$@" -C "$option" ;; esac
        ;;
      -Cincremental=*) ;;
      *) set -- "$@" "$argument" ;;
    esac
  done
fi
if [ "${CARGO_PRIMARY_PACKAGE:-}" = 1 ] && [ -n "${LOOM_ROOT_INCREMENTAL:-}" ]; then
  set -- "$@" -C "incremental=$LOOM_ROOT_INCREMENTAL"
fi
# Relative input paths also carry the compiler working directory in metadata.
# Remap both owner paths so relocating the definition preserves artifact bytes.
compiler_cwd=$(pwd -P)
set -- "$@" "--remap-path-prefix=$compiler_cwd=/loom/build" "--remap-path-prefix=${CARGO_MANIFEST_DIR:?missing package source}=/loom/source"
# CARGO_MANIFEST_DIR identifies package source, not the compiler's cwd.
export LOOM_RUSTC_CWD="$compiler_cwd"
capture=${LOOM_RUSTC_CAPTURE:?missing capture destination}
mkdir -p "$capture.units"
temporary="$capture.units/unit-$$.pending"
{
  env -0
  if [ "${DYLD_LIBRARY_PATH+x}" = x ]; then printf '%s\000' "DYLD_LIBRARY_PATH=$DYLD_LIBRARY_PATH"; fi
  printf '%s\000' LOOM_RUSTC_ARGUMENTS "$@"
} > "$temporary"
cache_status=3
if [ -n "${LOOM_COMPILER_CACHE_OWNER:-}" ] && [ "${CARGO_PRIMARY_PACKAGE:-}" != 1 ]; then
  if "$LOOM_COMPILER_CACHE_OWNER" __compiler-cache lookup "$temporary" "$LOOM_COMPILER_CACHE_MIRROR" 2> "$capture.units/unit-$$.stderr"; then
    cache_status=0
  else
    cache_status=$?
  fi
  if [ "$cache_status" != 0 ] && [ "$cache_status" != 3 ]; then
    cat "$capture.units/unit-$$.stderr" >&2
    exit "$cache_status"
  fi
fi
if [ "$cache_status" = 3 ]; then
  printf '%s\n' loom-rustc-invocation >&2
  if "$@" 2> "$capture.units/unit-$$.stderr"; then
    status=0
  else
    status=$?
  fi
  if [ "$status" -ne 0 ]; then
    cat "$capture.units/unit-$$.stderr" >&2
    exit "$status"
  fi
  if [ -n "${LOOM_COMPILER_CACHE_OWNER:-}" ] && [ "${CARGO_PRIMARY_PACKAGE:-}" != 1 ]; then
    "$LOOM_COMPILER_CACHE_OWNER" __compiler-cache record "$temporary" "$LOOM_COMPILER_CACHE_MIRROR"
  fi
  # Cargo may schedule dependents as soon as metadata is announced. Publish
  # their ownership keys before forwarding that announcement.
  cat "$capture.units/unit-$$.stderr" >&2
else
  cat "$capture.units/unit-$$.stderr" >&2
fi
mv "$temporary" "$capture.units/unit-$$.recipe"
if [ "${CARGO_PRIMARY_PACKAGE:-}" = 1 ]; then
  cp "$capture.units/unit-$$.recipe" "$capture"
fi
