#!/usr/bin/env bash
# Fails if a documented path does not exist.
#
# Two classes of reference rot are covered, because both have bitten before and
# neither is visible in a diff:
#
#   1. Relative Markdown links, resolved against the file that contains them.
#      The previous inline CI check only read README.md and docs/*.md, so the
#      fourteen ADRs under docs/adr/ - which cross-reference each other and the
#      documents above them - were unchecked, as were CONTRIBUTING.md and
#      SECURITY.md.
#
#   2. Repository paths named in source and script comments. This codebase
#      argues its decisions in doc comments and points at the document that
#      records them; a moved file silently turns seventeen of those into dead
#      ends. Nothing checked them at all before.
#
# Deliberately not checked: http(s) URLs. A link checker that reaches the
# network makes CI fail for reasons that have nothing to do with the change
# under test.
set -uo pipefail

cd "$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

missing=0

while IFS= read -r md; do
  dir=$(dirname "$md")
  while IFS= read -r link; do
    [ -n "$link" ] || continue
    [ -e "$dir/$link" ] || { echo "broken link: $md -> $link" >&2; missing=1; }
  done < <(grep -oE '\]\([^)]+\)' "$md" \
           | sed 's/^](//; s/)$//' \
           | grep -vE '^(https?:|mailto:|#)' \
           | sed 's/#.*//')
done < <(find . -name '*.md' -not -path './.git/*' -not -path '*/node_modules/*' \
           -not -path './.toolchain/*' -not -path './target/*')

# Vendored and generated trees are excluded the way the Markdown pass above
# already excludes them. Without this the scan reads every TypeScript file under
# the web app's installed dependencies, and any of them mentioning a path that
# happens to start with one of our top-level directory names fails the check for
# a file nobody here was ever meant to have. One in the TypeScript compiler's
# own sources did exactly that.
#
# It only appears once somebody runs `pnpm install`, so it stayed invisible
# while nobody had built the web UI, and would have broken CI the moment that
# step was wired in.
#
# Note for whoever edits this comment: this script is itself scanned, so an
# example path written out in full here would be reported as missing. That is
# not a bug in the check - the check has no way to know a path is an example -
# but it is a trap worth naming.
while IFS= read -r path; do
  [ -e "$path" ] || { echo "broken path reference in source: $path" >&2; missing=1; }
done < <(grep -ohrE '\b(docs|crates|apps|scripts|benchmarks|fixtures)/[A-Za-z0-9._/-]+\.(md|rs|ts|tsx|json|toml|sh|html)\b' \
           --include='*.rs' --include='*.sh' --include='*.toml' \
           --include='*.ts' --include='*.tsx' \
           --exclude-dir=node_modules --exclude-dir=dist \
           --exclude-dir=target --exclude-dir=.git --exclude-dir=.toolchain \
           . | sort -u)

if [ "$missing" -eq 0 ]; then
  echo "documentation links ok"
fi
exit "$missing"
