#!/bin/sh
# SPDX-License-Identifier: GPL-3.0-or-later
#
# NFR-SEC-02: fails when expose_secret() appears inside a formatting or logging macro, including a
# call spread over several lines. It cannot follow a value through a variable, so this catches
# mistakes, not intent. assert macros are left out: tests compare secrets, and library code has none.
# Covers tracked files and new files that git does not ignore. Run from anywhere in the repository.

set -eu
cd "$(git rev-parse --show-toplevel)"

files=$(git ls-files --cached --others --exclude-standard -- '*.rs')
found=0
for file in $files; do
    [ -f "$file" ] || continue # Deleted from the working tree but still in the index.
    awk '
        BEGIN {
            macros = "(^|[^a-zA-Z0-9_])(format|format_args|print|println|eprint|eprintln|write|writeln|" \
                "panic|todo|unimplemented|unreachable|trace|debug|info|warn|error|anyhow|bail)!"
        }
        /^[ \t]*\/\// { next } # Comments may name the things they warn about.
        {
            if (!inside && $0 ~ macros) { inside = 1; depth = 0 }
            if (!inside) { next }
            if (/expose_secret/) {
                printf "secret in a formatting macro: %s:%d: %s\n", FILENAME, FNR, $0
                bad = 1
            }
            # The macro ends where its brackets balance. String contents do not count.
            code = $0
            gsub(/"([^"\\]|\\.)*"/, "", code)
            depth += gsub(/\(/, "", code) - gsub(/\)/, "", code)
            if (depth <= 0) { inside = 0 }
        }
        END { exit bad }
    ' "$file" || found=1
done

[ "$found" -eq 0 ] && echo "no secrets in formatting macros"
exit "$found"
