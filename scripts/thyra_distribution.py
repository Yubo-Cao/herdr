#!/usr/bin/env python3
"""Generate the Thyra fork's update manifest from its release assets."""

import argparse
import hashlib
import json
import re
from pathlib import Path

from scripts.changelog import (
    EXPECTED_ASSET_NAMES,
    read_endpoint_protocol_generation,
    read_protocol_version,
)


def build_manifest(repo: str, tag: str, assets_dir: Path) -> dict:
    if not re.fullmatch(r"v[0-9]+\.[0-9]+\.[0-9]+-thyra\.[0-9]+", tag):
        raise ValueError("tag must look like v0.9.1-thyra.1")
    assets = {}
    checksums = {}
    for target, name in EXPECTED_ASSET_NAMES.items():
        with (assets_dir / name).open("rb") as asset:
            checksums[target] = hashlib.file_digest(asset, "sha256").hexdigest()
        assets[target] = f"https://github.com/{repo}/releases/download/{tag}/{name}"
    return {
        "version": tag.removeprefix("v"),
        "protocol": read_protocol_version(),
        "endpoint_generation": read_endpoint_protocol_generation(),
        "notes": f"Thyra fork release {tag}: https://github.com/{repo}/releases/tag/{tag}",
        "assets": assets,
        "sha256": checksums,
    }


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", required=True)
    parser.add_argument("--tag", required=True)
    parser.add_argument("--assets", type=Path, required=True)
    args = parser.parse_args()
    manifest = build_manifest(args.repo, args.tag, args.assets)
    (args.assets / "latest.json").write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")


if __name__ == "__main__":
    main()
