#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
image=${LOOM_IMAGE:-localhost/loom:local}
name=loom-container-01a084e8
volume=$name-data
if podman container exists "$name" || podman volume exists "$volume"; then
  echo "Container test resources already exist: $name" >&2
  exit 75
fi
policy_args=()
[[ -z ${LOOM_IMAGE_POLICY:-} ]] || policy_args+=(--signature-policy "$LOOM_IMAGE_POLICY")
podman build "${policy_args[@]}" --memory=8g --cpu-period=100000 --cpu-quota=400000 -t "$image" .
started=false
cleanup() {
  if [[ "$started" == true ]]; then podman stop --time 10 "$name"; fi
  podman volume rm "$volume"
}
trap cleanup EXIT INT TERM
export LOOM_TOKEN=loom-container-test LOOM_URL=http://127.0.0.1:18788
podman run --detach --rm --name "$name" --memory=8g --cpus=4 \
  -p 127.0.0.1:18788:8787 -e "LOOM_TOKEN=$LOOM_TOKEN" \
  -v "$volume:/data" "$image"
started=true
ready=false
for attempt in $(seq 1 100); do
  if bun -e 'fetch(process.env.LOOM_URL+"/health").then(r=>process.exit(r.ok?0:1)).catch(()=>process.exit(1))'; then ready=true; break; fi
  sleep .2
done
if [[ "$ready" != true ]]; then podman logs "$name"; exit 1; fi
if ! bun scripts/e2e.ts; then podman logs "$name"; exit 1; fi
if ! bun scripts/mcp-e2e.ts; then podman logs "$name"; exit 1; fi
printf 'Container HTTP and MCP smoke passed for %s\n' "$image"
