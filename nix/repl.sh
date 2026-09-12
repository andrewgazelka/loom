#!@bash@
set -euo pipefail
# The same first-argument check the launcher makes, so both agree on what asks
# for help and what asks for a daemon.
if [[ ${1:-} == --help || ${1:-} == -h ]]; then
  exec '@loomd@' "$@"
fi
bind=${LOOM_BIND:-127.0.0.1:8787}
next_is_bind=false
next_is_token=false
explicit_token=""
has_explicit_token=false
has_tokens_file=false
for argument in "$@"; do
  if $next_is_bind; then
    bind=$argument
    next_is_bind=false
    continue
  fi
  if $next_is_token; then
    explicit_token=$argument
    has_explicit_token=true
    next_is_token=false
    continue
  fi
  case "$argument" in
    --bind) next_is_bind=true ;;
    --bind=*) bind=${argument#--bind=} ;;
    --token) next_is_token=true ;;
    --token=*)
      explicit_token=${argument#--token=}
      has_explicit_token=true
      ;;
    --tokens-file|--tokens-file=*) has_tokens_file=true ;;
  esac
done
if [[ $(uname -s) == Darwin ]]; then
  data_home=${XDG_DATA_HOME:-"$HOME/Library/Application Support"}
else
  data_home=${XDG_DATA_HOME:-"$HOME/.local/share"}
fi
state=${LOOM_DATA_DIR:-"$data_home/loom"}
'@loomd@' "$@" &
pid=$!
trap 'kill "$pid" 2>/dev/null || true' EXIT
host=${bind%%:*}
port=${bind##*:}
listening=false
for _ in $(seq 1 50); do
  kill -0 "$pid" 2>/dev/null || break
  if (exec 3<>"/dev/tcp/$host/$port") 2>/dev/null; then
    exec 3>&- 3<&-
    listening=true
    break
  fi
  sleep 0.1
done
# A connection proves something answers that address, not that it is ours: when
# the address is already taken the squatter answers this probe and loomd exits a
# few tens of milliseconds later (35 ms, measured on macOS 27 against a held
# port). So the daemon's own liveness is the verdict, and it gets this long to
# fail after the address first answered; a dead daemon never gets a URL printed
# or a browser opened for it. A daemon that dies later still dies loudly: `wait`
# at the end of this script exits with its status.
sleep 0.5
if ! kill -0 "$pid" 2>/dev/null; then
  trap - EXIT
  status=0
  wait "$pid" || status=$?
  printf 'Loom: loomd exited with status %d; nothing from this run is serving %s.\n' "$status" "$bind" >&2
  exit "$status"
fi
if ! $listening; then
  printf 'Loom: still waiting for %s to accept connections; opening the browser anyway.\n' "$bind" >&2
fi
# The token rides the URL FRAGMENT, never a query string: fragments never
# reach the server or its access logs. `--tokens-file` names a multi-token
# file, not one bearer secret, so there is nothing to embed for it.
token=""
if $has_explicit_token; then
  token=$explicit_token
elif ! $has_tokens_file; then
  if [[ -n ${LOOM_TOKEN:-} ]]; then
    token=$LOOM_TOKEN
  else
    for _ in $(seq 1 50); do
      if [[ -s "$state/token" ]]; then
        token=$(cat "$state/token")
        break
      fi
      sleep 0.1
    done
  fi
fi
if [[ -n $token ]]; then
  url="http://$bind/#token=$token"
else
  url="http://$bind/"
fi
printf 'Loom REPL: %s\n' "$url" >&2
printf 'CLI: %s\n' '@cli@' >&2
# A browser that refuses to open is cosmetic; under `set -e` an unchecked
# failure here would abort the script and the EXIT trap would kill a daemon
# that is serving perfectly well.
if [[ $(uname -s) == Darwin ]] && command -v open >/dev/null 2>&1; then
  open "$url" || printf 'Loom: could not open a browser; open %s manually.\n' "$url" >&2
elif command -v xdg-open >/dev/null 2>&1 && { [[ -n ${DISPLAY:-} ]] || [[ -n ${WAYLAND_DISPLAY:-} ]]; }; then
  xdg-open "$url" || printf 'Loom: could not open a browser; open %s manually.\n' "$url" >&2
else
  printf 'Loom: no display or browser opener found; open %s manually.\n' "$url" >&2
fi
wait "$pid"
