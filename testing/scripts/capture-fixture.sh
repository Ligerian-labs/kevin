#!/usr/bin/env bash
# Capture a real worker CLI stream as a golden fixture (plan/11-testing.md).
#
# Usage: testing/scripts/capture-fixture.sh <claude|codex|pi|opencode> <prompt>
#
# Stub (WS-00): the real implementation lands with the worker adapters
# (WS-05/06/13/14/15). It must scrub tokens/keys/paths, store the JSONL under
# crates/kevin-worker/tests/fixtures/<kind>/ and write a sidecar .meta.toml with
# the CLI version.
set -euo pipefail
echo "capture-fixture.sh: not implemented yet (see plan/11-testing.md)" >&2
exit 2
