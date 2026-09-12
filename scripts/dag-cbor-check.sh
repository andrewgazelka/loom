#!/usr/bin/env bash
set -u
cd "$(dirname "$0")/.."
passed=0
first=''
check() {
  label=$1
  shift
  if "$@"; then
    passed=$((passed + 1))
  elif [[ -z "$first" ]]; then
    first=$label
  fi
}
check 'Rust codec' cargo test --locked -p loom-proto --test dag_cbor
check 'database migration' cargo test --locked -p loom-store --test dag_cbor_migration
check 'native Rust links' bash scripts/dag-cbor-e2e.sh
printf '%s/3 DAG-CBOR gates pass; first failing step: %s\n' "$passed" "${first:-none}"
[[ "$passed" -eq 3 ]]
