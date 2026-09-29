#!/usr/bin/env python3
"""Refresh the registered OpenCode Go catalog from the live API.

Fetches the model list from `{base}/models` (default
`https://opencode.ai/zen/go/v1/models`), compares it with the MODELS table in
`src/providers/opencode/model.rs`, and either reports the difference or
registers the missing IDs with one command:

    scripts/refresh-opencode-models.py --write

New entries get their wire protocol from the same family rules as
`infer_endpoint` in `model.rs` (minimax/qwen -> messages, grok/gpt/muse-spark
-> responses, everything else -> chat completions), which match the official
endpoint table at https://opencode.ai/docs/go/#endpoints. After writing, run
`cargo test` and spot-check new IDs against that table.

Routing does not depend on this script: any `opencode-go/<id>` is forwarded
upstream with an inferred protocol even when the catalog has never seen it.
The script only pins new IDs to their documented protocol.

Usage:
    scripts/refresh-opencode-models.py [--base-url URL] [--write] [--check]

    --write      insert missing models into model.rs
    --check      exit 1 when missing models exist (for CI); prints the diff
    --base-url   override the API base URL (else $CCP_OPENCODE_BASE_URL)
"""

import argparse
import json
import os
import re
import sys
import urllib.request

DEFAULT_BASE_URL = "https://opencode.ai/zen/go/v1"

ENTRY_RE = re.compile(
    r'id:\s*"([^"]+)",\s*\n\s*endpoint:\s*EndpointKind::(\w+)'
)

MODELS_START = "pub const MODELS: &[ModelSpec] = &["


def infer_endpoint(model_id):
    """Mirror of infer_endpoint() in model.rs. Keep the two in sync."""
    if model_id.startswith("minimax-") or model_id.startswith("qwen"):
        return "Messages"
    if (
        model_id.startswith("grok-")
        or model_id.startswith("gpt-")
        or model_id.startswith("muse-spark-")
    ):
        return "Responses"
    return "ChatCompletions"


def repo_path(*parts):
    here = os.path.dirname(os.path.abspath(__file__))
    return os.path.join(here, "..", *parts)


def fetch_upstream_ids(base_url):
    url = base_url.rstrip("/") + "/models"
    request = urllib.request.Request(
        url,
        headers={
            "Accept": "application/json",
            "User-Agent": "cc-proxy-model-refresh/1.0",
        },
    )
    with urllib.request.urlopen(request, timeout=30) as response:
        payload = json.load(response)
    try:
        ids = sorted({entry["id"] for entry in payload["data"]})
    except (KeyError, TypeError) as error:
        raise SystemExit(f"unexpected /models shape from {url}: {error}")
    return [model_id for model_id in ids if model_id]


def parse_local_catalog(path):
    with open(path, encoding="utf-8") as handle:
        text = handle.read()
    entries = dict(ENTRY_RE.findall(text))
    if not entries:
        raise SystemExit(f"no MODELS entries parsed from {path}")
    return text, entries


def insert_entries(text, additions):
    """Insert new ModelSpec entries before the closing `];` of MODELS."""
    start = text.index(MODELS_START)
    close = text.index("\n];", start)
    block = ""
    for model_id, endpoint in additions:
        block += (
            "    ModelSpec {\n"
            f'        id: "{model_id}",\n'
            f"        endpoint: EndpointKind::{endpoint},\n"
            "    },\n"
        )
    return text[:close] + "\n" + block.rstrip("\n") + "\n" + text[close:]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--base-url", default=None)
    parser.add_argument("--write", action="store_true")
    parser.add_argument("--check", action="store_true")
    args = parser.parse_args()

    base_url = (
        args.base_url or os.environ.get("CCP_OPENCODE_BASE_URL") or DEFAULT_BASE_URL
    )
    model_rs = repo_path("src", "providers", "opencode", "model.rs")
    upstream = fetch_upstream_ids(base_url)
    text, local = parse_local_catalog(model_rs)

    missing = [model_id for model_id in upstream if model_id not in local]
    stale = sorted(set(local) - set(upstream))

    print(f"upstream models: {len(upstream)}  local catalog: {len(local)}")
    if missing:
        print(f"\nmissing from catalog ({len(missing)}):")
        for model_id in missing:
            print(f"  {model_id:40} -> {infer_endpoint(model_id)}")
    else:
        print("\ncatalog covers every upstream model.")
    if stale:
        print(f"\nlocal-only IDs kept as-is ({len(stale)}):")
        for model_id in stale:
            print(f"  {model_id:40} ({local[model_id]})")
        print("Kept: the API sometimes serves IDs past their docs listing.")

    if args.check:
        if missing:
            print("\n--check: catalog is stale.", file=sys.stderr)
            return 1
        print("\n--check: catalog is current.")
        return 0

    if not missing:
        return 0
    if not args.write:
        print("\nRe-run with --write to register the missing IDs.")
        return 0

    additions = sorted((model_id, infer_endpoint(model_id)) for model_id in missing)
    updated = insert_entries(text, additions)
    with open(model_rs, "w", encoding="utf-8") as handle:
        handle.write(updated)
    # Verify the edit parses back to the expected catalog.
    _, reparsed = parse_local_catalog(model_rs)
    assert all(model_id in reparsed for model_id in missing), "write verification failed"
    print(f"\nregistered {len(additions)} model(s) in {model_rs}.")
    print("Next: verify new IDs against https://opencode.ai/docs/go/#endpoints,")
    print("extend the `refreshed_models_resolve_and_are_advertised` test,")
    print("then run: cargo test")
    return 0


if __name__ == "__main__":
    sys.exit(main())
