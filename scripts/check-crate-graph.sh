#!/usr/bin/env bash
# Safety rule: the MCP crate must never depend (even transitively) on the engine, removal or connector crates.
set -euo pipefail
deps=$(cargo tree -p nasbak-mcp --prefix none --edges normal | awk '{print $1}' | sort -u)
for forbidden in nasbak-engine nasbak-removal nasbak-connector nasbak-store; do
  if echo "$deps" | grep -qx "$forbidden"; then
    echo "FAIL: nasbak-mcp depends on $forbidden" >&2; exit 1
  fi
done
# Only nasbak-removal and the agent may use the Eraser trait.
bad=$(grep -rl "Eraser" crates --include=*.rs | grep -v -e nasbak-connector -e nasbak-removal || true)
if [ -n "$bad" ]; then echo "FAIL: Eraser used outside removal: $bad" >&2; exit 1; fi
echo "crate graph OK"
