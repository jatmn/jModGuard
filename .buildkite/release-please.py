#!/usr/bin/env python3
"""Open or merge Release Please Stable PRs from Buildkite. Nightlies stay separate."""

import json
import os
from pathlib import Path
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]


def load_config():
    config = json.loads((ROOT / "release-please-config.json").read_text())
    manifest = json.loads((ROOT / ".release-please-manifest.json").read_text())
    if config.get("release-type") != "simple" or config.get("include-v-in-tag") is not True:
        raise SystemExit("Release Please must manage v-prefixed Stable tags only")
    if "." not in manifest:
        raise SystemExit("Release Please manifest must version the repository root")
    return config, manifest


def main():
    load_config()
    if os.environ.get("BUILDKITE_PULL_REQUEST", "false") != "false":
        print("Release Please skipped on pull requests")
        return
    if os.environ.get("BUILDKITE_SOURCE") != "webhook" or os.environ.get("BUILDKITE_BRANCH") != "main":
        print("Release Please skipped outside trusted main pushes")
        return
    if os.environ.get("BUILDKITE_TAG"):
        print("Release Please skipped on existing tags")
        return
    token = os.environ.get("GITHUB_TOKEN") or ""
    if token in ("", "$GITHUB_TOKEN"):
        try:
            token = subprocess.run(
                ["buildkite-agent", "secret", "get", "GITHUB_TOKEN"],
                check=True, capture_output=True, text=True,
            ).stdout.strip()
        except (OSError, subprocess.CalledProcessError):
            token = ""
    if not token:
        print("Release Please skipped without GITHUB_TOKEN")
        return
    repo = os.environ.get("BUILDKITE_REPO") or "https://github.com/jatmn/jModGuard"
    for command in ("release-pr", "github-release"):
        subprocess.run(
            ["npx", "--yes", "release-please", command, f"--token={token}", f"--repo-url={repo}"],
            check=True, cwd=ROOT,
        )


if __name__ == "__main__":
    try:
        main()
    except (ValueError, json.JSONDecodeError) as error:
        sys.exit(f"Release Please failed: {error}")
