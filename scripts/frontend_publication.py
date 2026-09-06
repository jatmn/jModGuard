#!/usr/bin/env python3
"""Ops-owned publication receipt: asset hashes, never a source-commit checkpoint."""
from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
import re
import sys

FILES = ("index.html", "app.js", "style.css")
RECEIPT = ".jmodguard-publication.json"


def asset_hashes(root: str) -> dict[str, str]:
    result = {}
    for name in FILES:
        path = Path(root, name)
        if path.is_symlink() or not path.is_file():
            raise ValueError(f"{name} must be a regular asset")
        result[name] = hashlib.sha256(path.read_bytes()).hexdigest()
    return result


def valid_zone(zone: str) -> bool:
    return isinstance(zone, str) and re.fullmatch(r"[1-9][0-9]*", zone) is not None


def configured_zone() -> str:
    zone = os.environ.get("BUNNY_PULL_ZONE_ID", "")
    if not valid_zone(zone):
        raise ValueError("BUNNY_PULL_ZONE_ID must be a positive numeric Pull Zone ID")
    return zone


def parse(raw: bytes, zone: str) -> dict | None:
    try:
        data = json.loads(raw)
        if (not valid_zone(zone) or not isinstance(data, dict) or data.get("version") != 1
                or type(data.get("complete")) is not bool
                or data.get("pull_zone_id") != zone):
            return None
        files = data.get("files")
        if not isinstance(files, dict):
            return None
        if not files and not data["complete"]:
            return data
        if set(files) != set(FILES) or not all(
            isinstance(value, str) and re.fullmatch(r"[0-9a-f]{64}", value)
            for value in files.values()
        ):
            return None
        return data
    except (ValueError, UnicodeDecodeError):
        return None


def main() -> None:
    zone = configured_zone()
    if sys.argv[1:] == ["zone"]:
        print(zone)
        return
    mode, root, previous = sys.argv[1:]
    old = parse(Path(previous).read_bytes(), zone) if Path(previous).exists() else None
    files = asset_hashes(root) if mode == "complete" else (old["files"] if old else {})
    print(json.dumps({"version": 1, "complete": mode == "complete", "pull_zone_id": zone, "files": files}, sort_keys=True))


if __name__ == "__main__":
    main()
