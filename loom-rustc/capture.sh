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
  if [ "$argument" = --error-format=json ]; then compile=yes; break; fi
done
if [ "$compile" = no ]; then exec "$@"; fi
# Every user definition, including definition dependencies, gets a correctness
# lint. The isolated Wasm runtime remains the execution boundary.
case "${CARGO_PRIMARY_PACKAGE:-}:${CARGO_PKG_NAME:-}" in
  1:*|*:loom-definition-*) set -- "$@" -Funsafe-code ;;
esac
set -- "$@" "--remap-path-prefix=${CARGO_MANIFEST_DIR:?missing package source}=/loom/source"
capture=${LOOM_RUSTC_CAPTURE:?missing capture destination}
mkdir -p "$capture.units"
temporary="$capture.units/unit-$$.pending"
{
  env -0
  if [ "${DYLD_LIBRARY_PATH+x}" = x ]; then printf '%s\000' "DYLD_LIBRARY_PATH=$DYLD_LIBRARY_PATH"; fi
  printf '%s\000' LOOM_RUSTC_ARGUMENTS "$@"
} > "$temporary"
printf '%s\n' loom-rustc-invocation >&2
"$@"
mv "$temporary" "$capture.units/unit-$$.recipe"
if [ "${CARGO_PRIMARY_PACKAGE:-}" = 1 ]; then
  cp "$capture.units/unit-$$.recipe" "$capture"
fi
