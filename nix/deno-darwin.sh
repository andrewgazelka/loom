cd "$LOOM_IMPORT_ROOT"
exec /usr/bin/sandbox-exec -D "IMPORT_ROOT=$LOOM_IMPORT_ROOT" -f '@profile@' '@deno@' "$@"
