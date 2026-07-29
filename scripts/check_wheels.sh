#!/usr/bin/env bash
# Assert every expected ABI tag produced a wheel.
#
# maturin's interpreter discovery can quietly build a subset — a green job
# that ships fewer wheels than intended is worse than a red one, because it
# is only noticed by the user whose install falls back to the sdist.
#
# Usage: check_wheels.sh <dist-dir> <abi-tag>...
set -euo pipefail

dist=$1
shift

echo "Wheels in $dist:"
ls -1 "$dist"

missing=()
for tag in "$@"; do
    if ! ls "$dist"/*"-$tag-"*.whl >/dev/null 2>&1; then
        missing+=("$tag")
    fi
done

if [ ${#missing[@]} -ne 0 ]; then
    echo "::error::no wheel built for: ${missing[*]}"
    exit 1
fi

echo "ok: all ${#} expected tags present"
