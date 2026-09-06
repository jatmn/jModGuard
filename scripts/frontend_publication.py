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
        # Legacy pending receipts cannot prove a retry was CSS-only.
        requires_api = data.get("requires_api", not data["complete"])
        if type(requires_api) is not bool:
            return None
        data["requires_api"] = requires_api
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


def make_receipt(mode: str, root: str, previous: str, zone: str) -> dict:
    old = parse(Path(previous).read_bytes(), zone) if Path(previous).exists() else None
    desired = asset_hashes(root)
    complete = mode == "complete"
    requires_api = False
    if not complete:
        requires_api = old is None or (not old["complete"] and old["requires_api"])
        for name in ("index.html", "app.js"):
            # previous lives beside the actual pre-upload Storage backups.
            # Those bytes can differ even when the last completed hashes match
            # the selected source (repair or an interrupted A -> B -> A).
            backup = Path(previous).parent / name
            requires_api |= (
                not old or old["files"].get(name) != desired[name]
                or not backup.is_file()
                or hashlib.sha256(backup.read_bytes()).hexdigest() != desired[name]
            )
    return {"version": 1, "complete": complete, "pull_zone_id": zone,
            "files": desired if complete else (old["files"] if old else {}),
            "requires_api": bool(requires_api)}


def main() -> None:
    zone = configured_zone()
    if sys.argv[1:] == ["zone"]:
        print(zone)
        return
    mode, root, previous = sys.argv[1:]
    print(json.dumps(make_receipt(mode, root, previous, zone), sort_keys=True))


if __name__ == "__main__":
    main()
