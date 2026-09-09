#!@bash@
set -euo pipefail
export PATH='@runtimePath@':"${PATH:-}"
export SSL_CERT_FILE='@certificates@'
export NIX_SSL_CERT_FILE="$SSL_CERT_FILE"
export LOOM_ROOT='@sources@'
if [[ ${1:-} == --help || ${1:-} == -h ]]; then
  exec '@daemon@' "$@"
fi
if [[ $(uname -s) == Darwin ]]; then
  data_home=${XDG_DATA_HOME:-"$HOME/Library/Application Support"}
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
if [[ -z ${LOOM_TOKEN:-} ]]; then
  if [[ ! -s "$state/token" ]]; then
    token=$(od -An -N32 -tx1 /dev/urandom | tr -d ' \n')
    (set -o noclobber; printf '%s\n' "$token" > "$state/token") 2>/dev/null || true
  fi
  LOOM_TOKEN=$(cat "$state/token")
  export LOOM_TOKEN
fi
bind=${LOOM_BIND:-127.0.0.1:8787}
printf 'Loom: http://%s\nToken file: %s/token\n' "$bind" "$state" >&2
exec '@daemon@' --db "$state/loom.sqlite" --bind "$bind" "$@"
