#!/usr/bin/env bash
# Check public copy without changing input data or implementation comments.
set -euo pipefail

pattern='—|\\u\{0*2014\}|&mdash;|&#(0*8212|[xX]0*2014);'
scan() {
  local matches status=0
  matches=$(grep "$@") || status=$?
  if [ "$status" -gt 1 ]; then
    return "$status"
  fi
  printf '%s\n' "$matches"
}

# ponytail: skip whole-line comments, not Rust syntax. Use a Rust parser if block comments need exemptions.
rust=$(scan -rnE --include='*.rs' "$pattern" src)
docs=$(scan -rnE --include='*.md' --include='*.mdx' "$pattern" README.md CHANGELOG.md docs/src scripts/ccp)
shell=$(scan -nE "$pattern" scripts/ccp/install.sh)
hits=$(
  {
    printf '%s\n' "$rust" | grep -Ev '^[^:]+:[0-9]+:[[:space:]]*//' || true
    printf '%s\n' "$docs"
    printf '%s\n' "$shell" | grep -Ev '^[0-9]+:[[:space:]]*#' | sed 's|^|scripts/ccp/install.sh:|' || true
  } | grep -vE '^$|^scripts/ccp/install.sh:$' || true
)

if [ -n "$hits" ]; then
  printf '%s\n' "Em dash found in public copy. Use a period, comma, colon or parentheses:" "$hits" >&2
  exit 1
fi
printf '%s\n' "No em dash in public copy."
