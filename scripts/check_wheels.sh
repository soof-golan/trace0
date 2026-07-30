#!/usr/bin/env bash
set -euo pipefail

if [ $# -lt 2 ]; then
    echo "usage: $0 <dist-dir> <abi-tag>..." >&2
    exit 2
fi

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
