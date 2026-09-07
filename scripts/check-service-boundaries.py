#!/usr/bin/env python3
"""Fail CI if the control service acquires bot/API application dependencies."""
import pathlib
import subprocess

ROOT = pathlib.Path(__file__).resolve().parents[1]
FORBIDDEN = {"jmodguard", "jmodguard-bot", "jmodguard-api", "serenity"}


def main():
    # Historical schema helpers remain available for existing DB regressions;
    # production handlers, commands and HTTP paths must not resume collection.
    retired = ("record_message_activity(", "mark_edited_with_previous(",
               "mark_deleted_with_previous(", "mark_deleted_bulk(", "backfill_members(",
               "db::member_joined(", "db::member_left(", "db::upsert_user(",
               "db::bump_message(", "db::insert_message(", "db::purge_old_messages(",
               "db::search_members(", "db::member_row(")
    source = ROOT / "src"
    paths = [source / name for name in ("handlers.rs", "logging.rs", "automod.rs", "state.rs")]
    for directory in ("bin", "commands", "web"):
        paths.extend((source / directory).rglob("*.rs"))
    for path in paths:
        text = path.read_text()
        for forbidden in retired:
            if forbidden in text:
                raise SystemExit(f"Retired routine-history operation: {path}: {forbidden}")

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
