#!/usr/bin/env bash
# Prints the release notes for one version, taken from CHANGELOG.md.
#
# The changelog is the single record of what a release contains, so the notes
# are cut from it rather than written a second time in a workflow, where the two
# would drift. Run it locally to see exactly what a tag will publish.
#
# Usage:  scripts/release-notes.sh <version>      e.g. 0.2.0, not v0.2.0
#
# Exits non-zero if the changelog has no `## [<version>] — <date>` section: a
# release whose notes are missing has not been prepared, and publishing it with
# an empty body would hide that.
set -euo pipefail

VERSION="${1:?usage: scripts/release-notes.sh <version>}"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CHANGELOG="$ROOT/CHANGELOG.md"
REPO_URL="https://github.com/davabe/darcbench"

# GitHub rejects a release body over 125,000 characters. A long section is
# replaced by its summary (everything before its first `###`) and a link, rather
# than truncated mid-sentence.
LIMIT=120000

section="$(awk -v v="$VERSION" '
    $0 ~ "^## \\[" v "\\] " { found = 1; next }
    found && /^## \[/ { exit }
    found { print }
' "$CHANGELOG")"

if [ -z "${section//[[:space:]]/}" ]; then
    echo "CHANGELOG.md has no '## [$VERSION] — <date>' section" >&2
    exit 1
fi

link="$REPO_URL/blob/v$VERSION/CHANGELOG.md"
if [ "${#section}" -gt "$LIMIT" ]; then
    summary="$(printf '%s\n' "$section" | awk '/^### /{exit} {print}')"
    printf '%s\n\nThe full list of changes is too long for a release page: see [CHANGELOG.md](%s).\n' \
        "$summary" "$link"
else
    printf '%s\n\n---\nSource of these notes: [CHANGELOG.md](%s).\n' "$section" "$link"
fi
