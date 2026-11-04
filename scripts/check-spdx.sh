#!/bin/sh
# SPDX-License-Identifier: GPL-3.0-or-later
#
# NFR-LIC-01: fails when a .rs or .slint file does not start with the SPDX header.
# Covers tracked files and new files that git does not ignore. Run from anywhere in the repository.

set -eu
cd "$(git rev-parse --show-toplevel)"

header='// SPDX-License-Identifier: GPL-3.0-or-later'
checked=0
missing=0

files=$(git ls-files --cached --others --exclude-standard -- '*.rs' '*.slint')
for file in $files; do
    [ -f "$file" ] || continue # Deleted from the working tree but still in the index.
    checked=$((checked + 1))
    first=$(head -n 1 "$file" | tr -d '\r')
    if [ "$first" != "$header" ]; then
        echo "missing SPDX header: $file"
        missing=$((missing + 1))
    fi
done

echo "checked $checked files, $missing missing the header"
[ "$missing" -eq 0 ]
