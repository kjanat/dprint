#!/bin/sh
# Run from the repository root against the binary built in the FreeBSD VM.
set -eu

exe=$(realpath "${1:?Path to the dprint binary is required}")
smoke_dir=$(mktemp -d)
trap 'rm -rf "${smoke_dir}"' EXIT HUP INT TERM
export DPRINT_CACHE_DIR="${smoke_dir}/cache"

"${exe}" --version
cp crates/test-plugin/test_plugin.wasm "${smoke_dir}/test_plugin.wasm"
printf '%s\n' '{"plugins":["./test_plugin.wasm"]}' >"${smoke_dir}/wasm.json"
result=$(printf hello | "${exe}" fmt --config "${smoke_dir}/wasm.json" --stdin smoke.txt)
test "${result}" = hello_formatted
# A second process loads the compiled plugin from the cache.
result=$(printf hello | "${exe}" fmt --config "${smoke_dir}/wasm.json" --stdin smoke.txt)
test "${result}" = hello_formatted

printf '%s\n' '{"plugins":["https://plugins.dprint.dev/exec-0.7.3.json"],"exec":{"commands":[{"command":"tr a-z A-Z","exts":["up"]}]}}' >"${smoke_dir}/exec.json"
export DPRINT_CACHE_DIR="${smoke_dir}/schema-cache"
"${exe}" schema --config "${smoke_dir}/exec.json" >"${smoke_dir}/schema.json"
test -s "${smoke_dir}/schema.json"
result=$(printf hello | "${exe}" fmt --config "${smoke_dir}/exec.json" --stdin smoke.up)
test "${result}" = HELLO
