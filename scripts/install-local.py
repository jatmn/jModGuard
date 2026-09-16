#!/usr/bin/env python3
"""Create a disposable local install layout from public source artifacts."""
from __future__ import annotations

import argparse
import os
import shutil
import subprocess
import sys
from pathlib import Path


def conductor_bin() -> Path | None:
    env = os.environ.get("JMODGUARD_CONDUCTOR_BIN")
    if env:
        path = Path(env)
        return path if path.is_file() else None
    target = Path(os.environ.get("CARGO_TARGET_DIR", Path.cwd() / "target"))
    for name in ("debug/jmodguard-conductor", "release/jmodguard-conductor"):
        candidate = target / name
        if candidate.is_file():
            return candidate
    which = shutil.which("jmodguard-conductor")
    return Path(which) if which else None


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", required=True, help="absolute install root")
    args = parser.parse_args()
    root = Path(args.root).expanduser()
    if not root.is_absolute():
        raise SystemExit("install root must be an absolute path")
    binary = conductor_bin()
    if binary is None:
        raise SystemExit(
            "jmodguard-conductor not found. Build it with "
            "`cargo build --locked -p jmodguard-conductor` or set JMODGUARD_CONDUCTOR_BIN."
        )
    completed = subprocess.run(
        [str(binary), "init-local", "--root", str(root)],
        check=False,
    )
    if completed.returncode != 0:
        return completed.returncode
    print(f"Public local layout written to {root}", file=sys.stderr)
    print("Next: fill *.env.example locally and follow docs/INSTALL.md", file=sys.stderr)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
