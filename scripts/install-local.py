#!/usr/bin/env python3
"""Create a disposable local install layout from public source artifacts."""
from __future__ import annotations

import argparse
import os
import shutil
import subprocess
import sys
from pathlib import Path

CONDUCTOR = "jmodguard-conductor"


def conductor_bin() -> Path | None:
    env = os.environ.get("JMODGUARD_CONDUCTOR_BIN")
    candidates: list[Path] = []
    if env:
        candidates.append(Path(env))
    target = Path(os.environ.get("CARGO_TARGET_DIR", Path.cwd() / "target"))
    candidates.extend(
        target / name
        for name in (f"debug/{CONDUCTOR}", f"release/{CONDUCTOR}")
    )
    which = shutil.which(CONDUCTOR)
    if which:
        candidates.append(Path(which))
    for path in candidates:
        if path.is_file() and path.name == CONDUCTOR:
            return path
    return None


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
            f"{CONDUCTOR} not found. Build it with "
            f"`cargo build --locked -p jmodguard-conductor` or set JMODGUARD_CONDUCTOR_BIN."
        )
    completed = subprocess.run(
        [CONDUCTOR, "init-local", "--root", str(root)],
        executable=str(binary),
        check=False,
    )
    if completed.returncode != 0:
        return completed.returncode
    print(f"Public local layout written to {root}", file=sys.stderr)
    print("Next: fill *.env.example locally and follow docs/INSTALL.md", file=sys.stderr)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
