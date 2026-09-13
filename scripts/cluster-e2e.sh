#!/usr/bin/env bash
# Requires prebuilt loomd/loom binaries and the Rust guest compilation toolchain.
# Run from the repository root. No builds are performed by this harness.
set -euo pipefail

passed=0
node_one_pid=
node_two_pid=
work=$(mktemp -d "${TMPDIR:-/tmp}/loom-cluster-e2e.XXXXXX")
loomd_bin=${LOOMD_BIN:-target/debug/loomd}
loom_bin=${LOOM_BIN:-target/debug/loom}
token=cluster-e2e-user
# Ports are overridable so a gate can run beside another daemon on this host.
port_one=${LOOM_CLUSTER_PORT_ONE:-8801}
port_two=${LOOM_CLUSTER_PORT_TWO:-8802}
root=$PWD

cleanup() {
    result=$?
    trap - EXIT
    for pid in "$node_one_pid" "$node_two_pid"; do
        if [[ -n "$pid" ]]; then
            kill "$pid" 2>/dev/null || true
            wait "$pid" 2>/dev/null || true
        fi
    done
    printf '%s/5\n' "$passed"
    # A failed run retains its isolated state and logs for the gate owner to remove.
    if [[ "$result" == 0 && "$passed" == 5 ]]; then
        rm -rf "$work"
        exit 0
    fi
    printf 'cluster-e2e failed; logs and actor files: %s\n' "$work" >&2
    for node in n1 n2; do
        [[ -f "$work/$node/loomd.log" ]] || continue
        printf '--- %s daemon log (last 5 lines) ---\n' "$node" >&2
        tail -n 5 "$work/$node/loomd.log" >&2
    done
    exit 1
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

for executable in "$loomd_bin" "$loom_bin"; do
    [[ -x "$executable" ]] || { printf 'missing executable: %s\n' "$executable" >&2; exit 1; }
done
command -v jq >/dev/null
mkdir -p "$work/n1" "$work/n2" "$work/store"
umask 077
dd if=/dev/urandom of="$work/cluster.key" bs=32 count=1 2>/dev/null

start_node() {
    local id=$1 port=$2
    "$loomd_bin" --db "$work/$id/definitions.db" --actors-dir "$work/$id/actors" \
        --root "$root" --bind "127.0.0.1:$port" --node-id "$id" \
        --advertise "127.0.0.1:$port" --cluster-key-file "$work/cluster.key" \
        --store "local:$work/store" --token "$token" >"$work/$id/loomd.log" 2>&1 &
    started_pid=$!
}
cli() {
    local port=$1
    shift
    "$loom_bin" --url "http://127.0.0.1:$port" --token "$token" "$@"
}
ready() {
    local port=$1 pid=$2
    for ((attempt=0; attempt<120; attempt++)); do
        kill -0 "$pid" 2>/dev/null || return 1
        if cli "$port" nodes >"$work/ready-$port.json" 2>/dev/null; then
            jq -e '.ok == true' "$work/ready-$port.json" >/dev/null && return 0
        fi
        sleep 0.25
    done
    return 1
}

start_node n1 $port_one
node_one_pid=$started_pid
start_node n2 $port_two
node_two_pid=$started_pid
ready $port_one "$node_one_pid"
ready $port_two "$node_two_pid"
for port in $port_one $port_two; do
    cli "$port" nodes | jq -e '
        .ok == true and ([.result[] | select(.live) | .node_id] | sort == ["n1", "n2"])
    ' >/dev/null
done
passed=$((passed + 1))

# StoreRegistry only resolves admitted definitions, not native test builtins.
# Admit the maintained counter example identically to both definition stores;
# takeover must resolve the pinned behavior hash on either node.
for port in $port_one $port_two; do
    cli "$port" add "$root/examples/unison/counter.rs" --name cluster-counter | jq -e '.ok == true' >/dev/null
done
actor=$(cli $port_two spawn cluster-counter '{}' | jq -er 'select(.ok == true) | .result.id')
cli $port_two drain | jq -e '.ok == true' >/dev/null
initial=$(cli $port_two info "$actor" | jq -er 'select(.ok == true) | .result.cursor')
cli $port_one send "$actor" '{}' --key cross-node >"$work/send.json"
jq -e --argjson expected "$((initial + 1))" '.ok == true and .result.cursor == $expected' "$work/send.json" >/dev/null
passed=$((passed + 1))

cli $port_one whereis "$actor" | jq -e --arg addr "127.0.0.1:$port_two" '
    .ok == true and .result.remote.node_id == "n2" and .result.remote.addr == $addr
' >/dev/null
passed=$((passed + 1))

cli $port_one move "$actor" n1 | jq -e '.ok == true' >/dev/null
cli $port_one whereis "$actor" | jq -e '.ok == true and .result == "local"' >/dev/null
stale=("$work/n2/actors/$actor".stale.*.db)
[[ -f "${stale[0]}" ]]
passed=$((passed + 1))

before=$(cli $port_one info "$actor" | jq -er 'select(.ok == true) | .result.cursor')
kill -9 "$node_one_pid"
wait "$node_one_pid" 2>/dev/null || true
node_one_pid=
# Config::default().lease_ttl is ten seconds; allow one second beyond expiry.
sleep 11
cli $port_two send "$actor" '{}' --key after-owner-death | \
    jq -e --argjson expected "$((before + 1))" '.ok == true and .result.cursor == $expected' >/dev/null
cli $port_two info "$actor" | \
    jq -e --argjson expected "$((before + 1))" '.ok == true and .result.cursor == $expected' >/dev/null
passed=$((passed + 1))
