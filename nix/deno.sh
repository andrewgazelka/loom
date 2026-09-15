#!@bash@
set -euo pipefail
export DENO_NO_UPDATE_CHECK=1
export DENO_NO_PROMPT=1

if [[ ${1:-} == bundle ]]; then
  # Deno's native bundler does not honor ESBUILD_BINARY_PATH. Its exact helper
  # path is checked against upstream source by update-deno.py. Seed that path
  # with our pinned executable so a fresh tenant cache downloads no extra tool.
  : "${DENO_DIR:?Loom bundling requires an explicit isolated DENO_DIR}"
  : "${LOOM_IMPORT_ROOT:?Loom bundling requires an isolated admission directory}"
  LOOM_IMPORT_ROOT=$('@realpath@' "$LOOM_IMPORT_ROOT")
  DENO_DIR=$('@realpath@' "$DENO_DIR")
  case "$DENO_DIR/" in "$LOOM_IMPORT_ROOT/"*) ;; *) printf 'DENO_DIR must be inside LOOM_IMPORT_ROOT\n' >&2; exit 1 ;; esac
  export LOOM_IMPORT_ROOT DENO_DIR
  helper_dir="$DENO_DIR/dl/@helperCache@"
  '@mkdir@' -p "$helper_dir"
  temporary=$('@mktemp@' -d "$helper_dir/.loom-esbuild.XXXXXX")
  trap '"@rm@" -rf "$temporary"' EXIT
  '@ln@' -s '@esbuild@' "$temporary/helper"
  '@mv@' -Tf "$temporary/helper" "$helper_dir/@helperName@"
  '@rmdir@' "$temporary"
  trap - EXIT
@isolation@
fi
exec '@deno@' "$@"
