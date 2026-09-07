#!/usr/bin/env python3
"""Fail CI if the control service acquires bot/API application dependencies."""
import pathlib
import subprocess

ROOT = pathlib.Path(__file__).resolve().parents[1]
FORBIDDEN = {"jmodguard", "jmodguard-bot", "jmodguard-api", "serenity"}


def main():
    for package in ("jmodguard-runtime", "jmodguard-conductor"):
        result = subprocess.run(
            ["cargo", "tree", "--locked", "--offline", "-p", package,
             "--edges", "normal", "--prefix", "none", "--format", "{p}"],
            cwd=ROOT, check=True, text=True, capture_output=True,
        )
        dependencies = {line.split()[0] for line in result.stdout.splitlines() if line.strip()}
        forbidden = dependencies & FORBIDDEN
        if forbidden:
            raise SystemExit(f"{package} imports application dependencies: {sorted(forbidden)}")
        print(f"{package}: application boundary passes ({len(dependencies)} distinct package names)")


if __name__ == "__main__":
    main()
