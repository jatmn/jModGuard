#!/usr/bin/env python3
"""Stable/Nightly deployment manifests. CI-only; never imported by bot/API/conductor."""

from __future__ import annotations

import hashlib
import hmac
import json
import os
from pathlib import Path
import re
import time

FORMAT = "JMODGUARD-RELEASE-1"
INDEX_FORMAT = "JMODGUARD-RELEASE-INDEX-1"
TRUST_FORMAT = "JMODGUARD-RELEASE-TRUST-1"
REQUIRED = ("bot", "api", "conductor", "frontend")
SHA = re.compile(r"[0-9a-f]{40}")
DIGEST = re.compile(r"sha256:[0-9a-f]{64}")
KEY_ID = re.compile(r"[a-z0-9-]{2,32}")
STABLE_TAG = re.compile(r"^v[0-9]+\.[0-9]+\.[0-9]+$")
NIGHTLY_TAG = re.compile(r"^nightly-[0-9]{8}-[0-9a-f]{12}$")
HEX32 = re.compile(r"[0-9a-f]{64}")


def canonical(value):
    return json.dumps(value, separators=(",", ":"), sort_keys=True).encode()


def stable_tag(name):
    """Nightly identifiers must never enter Release Please's stable baseline."""
    return bool(STABLE_TAG.fullmatch(name or ""))


def nightly_tag(name):
    return bool(NIGHTLY_TAG.fullmatch(name or ""))


def channel_for(mode, tag=""):
    if mode == "nightly":
        return "nightly"
    if mode in {"tag", "manual"} and stable_tag(tag.removeprefix("refs/tags/")):
        return "stable"
    return None


def nightly_label(sha, when=None):
    if not SHA.fullmatch(sha):
        raise ValueError("nightly source must be a full commit SHA")
    day = time.strftime("%Y%m%d", time.gmtime(when or time.time()))
    return f"nightly-{day}-{sha[:12]}"


def frontend_inventory(root):
    files = {}
    for name in ("index.html", "app.js", "style.css"):
        path = Path(root, name)
        if path.is_symlink() or not path.is_file():
            raise ValueError(f"frontend {name} must be a regular file")
        files[name] = hashlib.sha256(path.read_bytes()).hexdigest()
    digest = hashlib.sha256("".join(files[name] for name in sorted(files)).encode()).hexdigest()
    return {"platform": "cdn", "sha256": "sha256:" + digest, "files": files}


def component(name, digest, size, image=None):
    if not DIGEST.fullmatch(digest):
        raise ValueError(f"{name} digest must be sha256:<64 hex>")
    if not isinstance(size, int) or size <= 0:
        raise ValueError(f"{name} size must be a positive integer")
    body = {"digest": digest, "platform": "linux/amd64", "size": size}
    if image:
        body["image"] = image
    return body


def build_unsigned(channel, release_id, sha, published_at, components, notes=""):
    if channel not in {"stable", "nightly"}:
        raise ValueError("channel must be stable or nightly")
    if channel == "stable" and not stable_tag(release_id):
        raise ValueError("stable release id must be a vMAJOR.MINOR.PATCH tag")
    if channel == "nightly" and not nightly_tag(release_id):
        raise ValueError("nightly release id must be nightly-YYYYMMDD-<sha12>")
    if not SHA.fullmatch(sha):
        raise ValueError("source_commit must be a full lowercase SHA")
    missing = [name for name in REQUIRED if name not in components]
    if missing:
        raise ValueError("incomplete publication; missing " + ",".join(missing))
    for name, body in components.items():
        if name not in REQUIRED or not isinstance(body, dict):
            raise ValueError(f"unexpected component {name}")
        if name == "frontend":
            files = body.get("files")
            if not isinstance(files, dict) or set(files) != {"index.html", "app.js", "style.css"}:
                raise ValueError("frontend inventory is incomplete")
            if not DIGEST.fullmatch(body.get("sha256", "")):
                raise ValueError("frontend sha256 is required")
        elif not DIGEST.fullmatch(body.get("digest", "")):
            raise ValueError(f"{name} digest is required")
    return {
        "format": FORMAT,
        "release_id": release_id,
        "channel": channel,
        "version": release_id.lstrip("v") if channel == "stable" else release_id,
        "source_commit": sha,
        "published_at": published_at,
        "components": components,
        "schema": {
            "core_family": "jmodguard-core-1",
            "core_version": 12,
            "guild_family": "jmodguard-guild-1",
            "guild_version": 8,
        },
        "settings_schema": 1,
        "backup_format": "JMODGUARD-BACKUP-1",
        "compatibility": {
            "min_conductor": "0.1.0",
            "rollback": "matching-backup",
        },
        "notes": notes,
    }


def sign(manifest, key_id, secret_hex):
    if not KEY_ID.fullmatch(key_id):
        raise ValueError("signing key id is invalid")
    if not HEX32.fullmatch(secret_hex):
        raise ValueError("signing key must be 32 bytes hex")
    body = {k: v for k, v in manifest.items() if k != "signature"}
    mac = hmac.new(bytes.fromhex(secret_hex), canonical(body), hashlib.sha256)
    signed = dict(body)
    signed["signature"] = {"alg": "hmac-sha256", "key_id": key_id, "mac": mac.hexdigest()}
    return signed


def verify(manifest, keys):
    signature = manifest.get("signature")
    if not isinstance(signature, dict) or signature.get("alg") != "hmac-sha256":
        raise ValueError("manifest is unsigned")
    key_id = signature.get("key_id")
    mac = signature.get("mac")
    if not KEY_ID.fullmatch(key_id or "") or not HEX32.fullmatch(mac or ""):
        raise ValueError("manifest signature is malformed")
    secret = keys.get(key_id)
    if not secret:
        raise ValueError("manifest signed with an unknown or retired key")
    expected = sign(manifest, key_id, secret)["signature"]["mac"]
    if not hmac.compare_digest(expected, mac):
        raise ValueError("manifest signature mismatch")
    return {k: v for k, v in manifest.items() if k != "signature"}


def load_trust(path):
    data = json.loads(Path(path).read_text())
    if not isinstance(data, dict) or data.get("format") != TRUST_FORMAT:
        raise ValueError("release trust file is invalid")
    keys = {}
    for item in data.get("keys") or []:
        if not isinstance(item, dict) or item.get("alg") != "hmac-sha256":
            raise ValueError("release trust entry is invalid")
        key_id = item.get("id")
        secret = item.get("secret")
        if not KEY_ID.fullmatch(key_id or "") or not HEX32.fullmatch(secret or ""):
            raise ValueError("release trust key is invalid")
        if not item.get("retired"):
            keys[key_id] = secret
    return keys


def index_releases(rows, retain=32):
    if retain < 1:
        raise ValueError("retention must be at least one complete release")
    by_channel = {"stable": [], "nightly": []}
    for row in rows:
        channel = row["channel"]
        if channel not in by_channel:
            raise ValueError("index contains an unknown channel")
        by_channel[channel].append(row)
    kept = []
    for channel, items in by_channel.items():
        items.sort(key=lambda row: row["published_at"], reverse=True)
        kept.extend(items[:retain])
    kept.sort(key=lambda row: row["published_at"], reverse=True)
    return {"format": INDEX_FORMAT, "releases": kept}


def stage_write(directory, identity, payload):
    root = Path(directory)
    root.mkdir(parents=True, exist_ok=True)
    staged = root / f".{identity}.staging.json"
    final = root / f"{identity}.json"
    staged.write_bytes(canonical(payload) + b"\n")
    os.replace(staged, final)
    return final


def refuse_partial(components):
    present = [name for name in REQUIRED if name in components]
    if len(present) != len(REQUIRED):
        raise ValueError("failed/partial publication remains unavailable")
    return True


def release_please_ignores_nightly(tags):
    return [tag for tag in tags if stable_tag(tag)]


def load_release_please(path):
    data = json.loads(Path(path).read_text())
    if not isinstance(data, dict) or data.get("release-type") != "simple":
        raise ValueError("Release Please config must use simple versioning")
    if data.get("include-v-in-tag") is not True:
        raise ValueError("stable tags must include the v prefix")
    packages = data.get("packages")
    if not isinstance(packages, dict) or "." not in packages:
        raise ValueError("Release Please must version the repository root")
    return data


def finalize(channel, release_id, sha, components, notes, key_id, secret, out_dir):
    refuse_partial(components)
    unsigned = build_unsigned(channel, release_id, sha, time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()), components, notes)
    signed = sign(unsigned, key_id, secret)
    path = stage_write(out_dir, release_id, signed)
    index_path = Path(out_dir) / "index.json"
    rows = []
    if index_path.exists():
        existing = json.loads(index_path.read_text())
        rows = list(existing.get("releases") or [])
        rows = [row for row in rows if row.get("release_id") != release_id]
    rows.append({
        "release_id": release_id,
        "channel": channel,
        "source_commit": sha,
        "published_at": signed["published_at"],
        "path": path.name,
    })
    index_path.write_bytes(canonical(index_releases(rows)) + b"\n")
    return path


def generate_hmac_key():
    return os.urandom(32).hex()


if __name__ == "__main__":
    raise SystemExit("use as a library from publication jobs and tests")
