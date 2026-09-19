#!/usr/bin/env bash
# Dump Brush's full CLI help so the training-parameter set can be checked
# against the shipped binary instead of against documentation.
#
# The engine manifest locks Brush 0.3.0 and the pipeline may only use flags that
# this binary really exposes, so the dump is the audit evidence for every Brush
# argument OOOSplat passes.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# Each platform ships the same Brush version from a different archive, so the
# binary lives in a different place: the Windows zip installs to engines/brush,
# the Linux tarball to engines/linux/brush (manifest.linux.json "destination"),
# and the macOS closure to engines/macos/arm64 with bin/ (manifest.macos.json
# "destination" plus its requiredFiles entry "bin/brush_app").
case "$(uname -s)" in
    Darwin) exe="$root/engines/macos/arm64/bin/brush_app" ;;
    Linux)  exe="$root/engines/linux/brush/brush_app" ;;
    *)      exe="$root/engines/brush/brush_app.exe" ;;
esac

output="$root/docs/brush_help.txt"

if [ ! -x "$exe" ]; then
    echo "Brush binary not found or not executable. Tried:" >&2
    echo "  $root/engines/macos/arm64/bin/brush_app" >&2
    echo "  $root/engines/linux/brush/brush_app" >&2
    echo "  $root/engines/brush/brush_app.exe" >&2
    echo "Install this platform's engines first (npm run setup:engines)." >&2
    exit 1
fi

mkdir -p "$(dirname "$output")"
"$exe" --help >"$output" 2>&1

echo "Wrote $output"
