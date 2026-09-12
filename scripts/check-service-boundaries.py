#!/usr/bin/env python3
"""Guard runtime state ownership and the control service dependency boundary."""
import pathlib
import re
import subprocess

ROOT = pathlib.Path(__file__).resolve().parents[1]
SERVICES = {"jmodguard-bot", "jmodguard-api", "jmodguard-conductor", "jmodguard-integration-tests"}
BOT_STACK = {"serenity", "tokio-tungstenite", "tungstenite"}
API_STACK = {"axum", "tower-sessions", "tower-sessions-core", "tower-sessions-memory-store"}


def check_tree(package, forbidden, flags=()):
    result = subprocess.run(
        ["cargo", "tree", "--locked", "--offline", "-p", package, *flags,
         "--edges", "normal,build", "--prefix", "none", "--format", "{p}"],
        cwd=ROOT, check=True, text=True, capture_output=True,
    )
    names = {line.split()[0] for line in result.stdout.splitlines() if line.strip()}
    found = names & forbidden
    if found:
        raise SystemExit(f"{package}: forbidden production dependencies: {sorted(found)}")
    print(f"{package} {' '.join(flags)}: dependency boundary passes ({len(names)} package names)")


def main():
    # The API must not regain access to the bot state or compiled guild cache.
    # The Rust state types/compile-fail docs provide the corresponding field boundary.
    api_paths = list((ROOT / "crates/api/src").rglob("*.rs"))
    for path in api_paths:
        if path.name == "enrollment_tests.rs":
            continue
        source = path.read_text()
        for forbidden in ("crate::state::", "jmodguard::state::", "jmodguard_bot::", "enrollment_repair::",
                          "enroll_guild(", "retire_guild_enrollment(", "nickname_worker::", "action_worker::", "discord_http", "serenity::http", "cfg.discord"):
            if forbidden in source:
                raise SystemExit(f"Bot state dependency in API: {path}: {forbidden}")
    for name in ("crates/bot/src/state.rs", "crates/bot/src/main.rs"):
        source = (ROOT / name).read_text()
        for forbidden in ("crate::web::", "jmodguard::web::", "jmodguard_api::"):
            if forbidden in source:
                raise SystemExit(f"Dashboard state dependency in bot: {name}: {forbidden}")
    print("Bot/API runtime state boundaries pass")

    # API may admit/read/cancel captured jobs; only the bot owns dispatch and cleanup.
    forbidden_api = re.compile(r"\b(?:enqueue_bot_action|bot_action|recent_bot_actions|cancel_bot_action|claim_bot_action|begin_bot_action|finish_bot_action|finish_dispatched_bot_action|bot_action_can_dispatch|prune_bot_action_history)\b")
    for path in api_paths:
        if path.name != "enrollment_tests.rs" and forbidden_api.search(path.read_text()):
            raise SystemExit(f"Unscoped queue access or worker operation in API: {path}")
    print("API captured admission and bot dispatch boundaries pass")
    unscoped_moderation = re.compile(r"\b(?:query_events|list_bans|guild_stats)\s*\(")
    for path in api_paths:
        if path.name != "enrollment_tests.rs" and unscoped_moderation.search(path.read_text()):
            raise SystemExit(f"Unscoped moderation read in API: {path}")
    print("API moderation reads retain captured membership")

    for path in api_paths:
        if path.name == "enrollment_tests.rs":
            continue
        # Handler names intentionally match some DB operation names; inspect qualified DB references.
        source = path.read_text()
        if re.search(r"\bdb::(?:get_rule|list_rules|list_badwords|list_commands|get_command_row|list_templates|get_template_row|list_log_channels|load_web_config_snapshot)\b", source):
            raise SystemExit(f"Unscoped configuration read in API: {path}")
    print("API configuration reads retain captured membership")

    # Captured configuration primitives are not API authorization on their own.
    # DB/API adoption includes admission and browser tokens; bot paths remain separate.
    scoped_config = re.compile(r"\b(?:ConfigurationScope|ConfigurationWriteScope|patch_enrolled_settings|copy_enrolled_guild_config|(?:upsert|delete)_enrolled_(?:rule|command|template)|(?:add|delete)_enrolled_badword)\b")
    for path in list((ROOT / "src").rglob("*.rs")) + list((ROOT / "crates/bot/src").rglob("*.rs")):
        if path.is_relative_to(ROOT / "src/db"):
            continue
        if scoped_config.search(path.read_text()):
            raise SystemExit(f"Captured configuration outside the DB/API boundary: {path}")
    print("Captured configuration is confined to DB/API code")

    # Catalog writes stay in the shared data crate. Bot/API may route through
    # TenantRuntime for live tenant data. The conductor still must not activate it.
    catalog = re.compile(r"\b(?:register_store|assign_initial_placement|clear_erased_placement|GuildStoreOptions)\b")
    for path in (ROOT / "crates/conductor/src").rglob("*.rs"):
        if catalog.search(path.read_text()):
            raise SystemExit(f"Guild-store catalog runtime activation: {path}")
    runtime = re.compile(r"\b(?:TenantRuntime|GuildDataStore|GuildDataScope|LocalEvidenceStore|BunnyEvidenceStore)\b")
    for path in (ROOT / "crates/conductor/src").rglob("*.rs"):
        if runtime.search(path.read_text()):
            raise SystemExit(f"Guild-store runtime in conductor: {path}")
    print("Guild-store catalog remains inactive in the conductor")

    # Bot-owned Discord mutations record their classified application case and
    # suppress the matching gateway echo. Unmarked manual/external events remain cases.
    handlers = (ROOT / "crates/bot/src/handlers.rs").read_text()
    for event in ("guild_ban_addition", "guild_ban_removal"):
        function = re.search(
            rf"async fn {event}\b.*?(?=\n    async fn |\n\}})", handlers, re.DOTALL
        )
        if function is None:
            raise SystemExit(f"Missing gateway handler: {event}")
        if ("consume_gateway_case_suppression" not in function.group()
                or "cases::record" not in function.group()):
            raise SystemExit(f"Gateway {event} must suppress bot echoes and retain external cases")
    print("Gateway ban events suppress bot echoes and retain external cases")

    check_tree("jmodguard", SERVICES | BOT_STACK | API_STACK)
    check_tree("jmodguard-bot", (SERVICES - {"jmodguard-bot"}) | API_STACK)
    for flags in ((), ("--no-default-features",)):
        check_tree("jmodguard-api", (SERVICES - {"jmodguard-api"}) | BOT_STACK, flags)
    check_tree("jmodguard-runtime", SERVICES | BOT_STACK | API_STACK | {"jmodguard"})
    check_tree("jmodguard-conductor", (SERVICES - {"jmodguard-conductor"}) | BOT_STACK | {"jmodguard"})


if __name__ == "__main__":
    main()
