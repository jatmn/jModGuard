#!/usr/bin/env python3
"""Guard runtime state ownership and the control service dependency boundary."""
import pathlib
import subprocess

ROOT = pathlib.Path(__file__).resolve().parents[1]
FORBIDDEN = {"jmodguard", "jmodguard-bot", "jmodguard-api", "serenity"}


def main():
    # The API must not regain access to the bot state or compiled guild cache.
    # The Rust state types/compile-fail docs provide the corresponding field boundary.
    api_paths = list((ROOT / "src/web").rglob("*.rs")) + [ROOT / "src/bin/jmodguard-api.rs"]
    for path in api_paths:
        source = path.read_text()
        for forbidden in ("crate::state::", "jmodguard::state::"):
            if forbidden in source:
                raise SystemExit(f"Bot state dependency in API: {path}: {forbidden}")
    for name in ("src/state.rs", "src/bin/jmodguard-bot.rs"):
        source = (ROOT / name).read_text()
        for forbidden in ("crate::web::", "jmodguard::web::"):
            if forbidden in source:
                raise SystemExit(f"Dashboard state dependency in bot: {name}: {forbidden}")
    print("Bot/API runtime state boundaries pass")

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
