#!/usr/bin/env bash
# Dump Brush's full CLI help so the training-parameter set can be checked
# against the shipped binary instead of against documentation.
#
# The engine manifest locks Brush 0.3.0 and the pipeline may only use flags that
# this binary really exposes, so the dump is the audit evidence for every Brush
# argument OOOSplat passes.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

case "$(uname -s)" in
    Darwin) exe="$root/engines/brush/brush_app" ;;
    *)      exe="$root/engines/brush/brush_app" ;;
esac

output="$root/docs/brush_help.txt"

if [ ! -x "$exe" ]; then
    echo "Brush binary not found or not executable: $exe" >&2
    exit 1
fi

mkdir -p "$(dirname "$output")"
"$exe" --help >"$output" 2>&1

echo "Wrote $output"
