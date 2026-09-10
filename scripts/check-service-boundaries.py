#!/usr/bin/env python3
"""Guard runtime state ownership and the control service dependency boundary."""
import pathlib
import re
import subprocess

ROOT = pathlib.Path(__file__).resolve().parents[1]
FORBIDDEN = {"jmodguard", "jmodguard-bot", "jmodguard-api", "serenity"}


def main():
    # The API must not regain access to the bot state or compiled guild cache.
    # The Rust state types/compile-fail docs provide the corresponding field boundary.
    api_paths = list((ROOT / "src/web").rglob("*.rs")) + [ROOT / "src/bin/jmodguard-api.rs"]
    for path in api_paths:
        if path.name == "enrollment_tests.rs":
            continue
        source = path.read_text()
        for forbidden in ("crate::state::", "jmodguard::state::", "enrollment_repair::",
                          "enroll_guild(", "retire_guild_enrollment(", "nickname_worker::", "action_worker::"):
            if forbidden in source:
                raise SystemExit(f"Bot state dependency in API: {path}: {forbidden}")
    for name in ("src/state.rs", "src/bin/jmodguard-bot.rs"):
        source = (ROOT / name).read_text()
        for forbidden in ("crate::web::", "jmodguard::web::"):
            if forbidden in source:
                raise SystemExit(f"Dashboard state dependency in bot: {name}: {forbidden}")
    print("Bot/API runtime state boundaries pass")

    # Queue storage is intentionally inactive until enrollment, authorization and
    # dispatch/outcome fencing land together. DB/dispatch libraries and tests are allowed;
    # runtime consumers must be an explicit change to this boundary.
    queue_names = re.compile(r"\b(?:enqueue_enrolled_bot_action|enrolled_bot_action|recent_enrolled_bot_actions|cancel_enrolled_bot_action|bot_action_can_dispatch|enqueue_bot_action|claim_bot_action|begin_bot_action|finish_bot_action|finish_dispatched_bot_action|cancel_bot_action|prune_bot_action_history|recent_bot_actions|bot_action|bot_actions)\b")
    for path in (ROOT / "src").rglob("*.rs"):
        relative = path.relative_to(ROOT / "src")
        if relative.parts[0] == "db" or relative.as_posix() in {"db.rs", "bot_actions.rs", "lib.rs", "action_worker.rs"} or relative.parts[0] == "action_worker":
            continue
        if queue_names.search(path.read_text()):
            raise SystemExit(f"Inactive queue consumed by runtime code: {path}")
    for path in (ROOT / "src").rglob("*.rs"):
        relative = path.relative_to(ROOT / "src")
        if relative.as_posix() in {"lib.rs", "action_worker.rs"} or relative.parts[0] == "action_worker":
            continue
        if re.search(r"\baction_worker\b", path.read_text()):
            raise SystemExit(f"Inactive dispatch worker consumed by runtime code: {path}")
    print("Action queue and dispatch library remain unscheduled")

    # Captured configuration primitives are not API authorization on their own.
    # DB/API adoption includes admission and browser tokens; bot paths remain separate.
    scoped_config = re.compile(r"\b(?:ConfigurationScope|ConfigurationWriteScope|patch_enrolled_settings|copy_enrolled_guild_config|(?:upsert|delete)_enrolled_(?:rule|command|template)|(?:add|delete)_enrolled_badword)\b")
    for path in (ROOT / "src").rglob("*.rs"):
        if path.relative_to(ROOT / "src").parts[0] in {"db", "web"}:
            continue
        if scoped_config.search(path.read_text()):
            raise SystemExit(f"Captured configuration outside the DB/API boundary: {path}")
    print("Captured configuration is confined to DB/API code")

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
