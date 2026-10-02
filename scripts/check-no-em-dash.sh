#!/usr/bin/env bash
# No em dash in the text readers see. An em dash gives away AI-written text.
# Checks the Rust sources (strings only: whole-line // comments are skipped),
# the READMEs, the docs site and the ccp helper. Tests, CHANGELOG and this
# script are exempt. Run from the repo root.
set -euo pipefail

dash=$'—'
hits=$(
  {
    grep -rn --include='*.rs' "$dash" src | grep -Ev '^[^:]+:[0-9]+:[[:space:]]*//' || true
    grep -rn --include='*.md' --include='*.mdx' "$dash" README.md docs/src scripts/ccp || true
    grep -n "$dash" scripts/ccp/install.sh | grep -Ev '^[0-9]+:[[:space:]]*#' | sed 's|^|scripts/ccp/install.sh:|' || true
  }
)

if [ -n "$hits" ]; then
  echo "Em dash found in user-facing text. Use a period, comma, colon or parentheses:" >&2
  echo "$hits" >&2
  exit 1
fi
echo "no em dash in user-facing text"
