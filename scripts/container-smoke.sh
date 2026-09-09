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
if [[ ${LOOM_SKIP_CONTAINER_BUILD:-0} != 1 ]]; then
  podman build "${policy_args[@]}" --memory=8g --cpu-period=100000 --cpu-quota=400000 -t "$image" .
fi
started=false
cleanup() {
  result=$?
  trap - EXIT INT TERM
  if [[ "$started" == true ]]; then
    podman stop --time 10 "$name" || result=1
    stopped_code=$(podman wait "$name") || result=1
    if [[ ${stopped_code:-missing} != 0 ]]; then
      echo "Container shutdown failed: exit=${stopped_code:-missing}" >&2
      result=1
    else
      echo 'Container SIGTERM shutdown exit=0'
    fi
    podman rm "$name" || result=1
  fi
  podman volume rm "$volume" || result=1
  exit "$result"
}
trap cleanup EXIT INT TERM
export LOOM_TOKEN=loom-container-test LOOM_URL=http://127.0.0.1:18788
podman run --detach --name "$name" --memory=8g --cpus=4 --security-opt "unmask=/proc/*" \
  -p 127.0.0.1:18788:8787 -e "LOOM_TOKEN=$LOOM_TOKEN" \
  -v "$volume:/data" "$image"
started=true
ready=false
for attempt in $(seq 1 100); do
  if bun -e 'fetch(process.env.LOOM_URL+"/health").then(r=>process.exit(r.ok?0:1)).catch(()=>process.exit(1))'; then ready=true; break; fi
  sleep .2
done
if [[ "$ready" != true ]]; then podman logs "$name"; exit 1; fi
if [[ ${LOOM_CONTAINER_SUITE:-all} == all ]]; then
  if ! bun scripts/e2e.ts; then podman logs "$name"; exit 1; fi
  if ! bun scripts/mcp-e2e.ts; then podman logs "$name"; exit 1; fi
fi
if [[ ${LOOM_CONTAINER_SUITE:-all} != vendor ]]; then
  if ! bun scripts/site-e2e.ts; then podman logs "$name"; exit 1; fi
fi
if [[ ${LOOM_CONTAINER_SUITE:-all} != site ]]; then
if ! bun scripts/container-vendor-smoke.ts; then podman logs "$name"; exit 1; fi
if ! podman exec --workdir /opt/loom "$name" bash loom-rustc/test-sandbox.sh; then podman logs "$name"; exit 1; fi
fi
printf 'Container %s smoke passed for %s\n' "${LOOM_CONTAINER_SUITE:-all}" "$image"
