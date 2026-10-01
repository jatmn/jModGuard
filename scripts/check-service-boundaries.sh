#!/usr/bin/env bash
# Guard runtime state ownership and the shared runtime dependency boundary.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"

fail() {
  echo "check-service-boundaries: $*" >&2
  exit 1
}

api_paths=()
while IFS= read -r -d '' path; do
  api_paths+=("$path")
done < <(find "$root/crates/api/src" -name '*.rs' -print0 | sort -z)

for path in "${api_paths[@]}"; do
  [[ "$(basename "$path")" == enrollment_tests.rs ]] && continue
  for forbidden in \
    'crate::state::' \
    'jmodguard::state::' \
    'jmodguard_bot::' \
    'enrollment_repair::' \
    'enroll_guild(' \
    'retire_guild_enrollment(' \
    'nickname_worker::' \
    'action_worker::' \
    'discord_http' \
    'serenity::http' \
    'cfg.discord'
  do
    if grep -Fq -- "$forbidden" "$path"; then
      fail "Bot state dependency in API: ${path#"$root"/}: $forbidden"
    fi
  done
done

for name in crates/bot/src/state.rs crates/bot/src/main.rs; do
  for forbidden in 'crate::web::' 'jmodguard::web::' 'jmodguard_api::'; do
    if grep -Fq -- "$forbidden" "$root/$name"; then
      fail "Dashboard state dependency in bot: $name: $forbidden"
    fi
  done
done
echo 'Bot/API runtime state boundaries pass'

forbidden_api='\<(enqueue_bot_action|bot_action|recent_bot_actions|cancel_bot_action|claim_bot_action|begin_bot_action|finish_bot_action|finish_dispatched_bot_action|bot_action_can_dispatch|bot_action_dispatch|prune_bot_action_history)\>'
for path in "${api_paths[@]}"; do
  [[ "$(basename "$path")" == enrollment_tests.rs ]] && continue
  if grep -Eq -- "$forbidden_api" "$path"; then
    fail "Unscoped queue access or worker operation in API: ${path#"$root"/}"
  fi
done
echo 'API captured admission and bot dispatch boundaries pass'

unscoped_moderation='\<(query_events|list_bans|guild_stats)[[:space:]]*\('
for path in "${api_paths[@]}"; do
  [[ "$(basename "$path")" == enrollment_tests.rs ]] && continue
  if grep -Eq -- "$unscoped_moderation" "$path"; then
    fail "Unscoped moderation read in API: ${path#"$root"/}"
  fi
done
echo 'API moderation reads retain captured membership'

unscoped_config='\<db::(get_rule|list_rules|list_badwords|list_commands|get_command_row|list_templates|get_template_row|list_log_channels|load_web_config_snapshot)\>'
for path in "${api_paths[@]}"; do
  [[ "$(basename "$path")" == enrollment_tests.rs ]] && continue
  if grep -Eq -- "$unscoped_config" "$path"; then
    fail "Unscoped configuration read in API: ${path#"$root"/}"
  fi
done
echo 'API configuration reads retain captured membership'

scoped_config='\<(ConfigurationScope|ConfigurationWriteScope|patch_enrolled_settings|copy_enrolled_guild_config|(upsert|delete)_enrolled_(rule|command|template)|(add|delete)_enrolled_badword)\>'
while IFS= read -r -d '' path; do
  rel="${path#"$root"/}"
  case "$rel" in
    src/db/*) continue ;;
  esac
  if grep -Eq -- "$scoped_config" "$path"; then
    fail "Captured configuration outside the DB/API boundary: $rel"
  fi
done < <(find "$root/src" "$root/crates/bot/src" -name '*.rs' -print0 | sort -z)
echo 'Captured configuration is confined to DB/API code'

extract_handler() {
  local event="$1"
  awk -v event="$event" '
    $0 ~ "^    async fn " event "\\(" { grab = 1 }
    grab && seen && $0 ~ /^    async fn / { exit }
    grab {
      print
      seen = 1
    }
    grab && /^}/ { exit }
  ' "$root/crates/bot/src/handlers.rs"
}

for event in guild_ban_addition guild_ban_removal; do
  function="$(extract_handler "$event")"
  [[ -n "$function" ]] || fail "Missing gateway handler: $event"
  printf '%s\n' "$function" | grep -Fq 'consume_gateway_case_suppression' \
    || fail "Gateway $event must suppress bot echoes and retain external cases"
  printf '%s\n' "$function" | grep -Fq 'cases::record' \
    || fail "Gateway $event must suppress bot echoes and retain external cases"
done
echo 'Gateway ban events suppress bot echoes and retain external cases'

# Audit enrichment may wait for Discord. Decide whether BanAdd is a bot echo
# and record an external case before that wait can expire the suppression.
extract_handler guild_ban_addition | awk '
  /consume_gateway_case_suppression/ { suppression = NR }
  /crate::cases::record\(/ { external_case = NR }
  /logging::lookup_member_audit\(/ { audit = NR }
  END { exit !(suppression && external_case && audit && suppression < audit && external_case < audit) }
' || fail 'Gateway ban case routing must precede audit lookup'

check_tree() {
  local package="$1"
  shift
  local -a flags=()
  local -a forbidden=()
  local seeing_forbidden=0
  local arg
  for arg in "$@"; do
    if [[ "$arg" == -- ]]; then
      seeing_forbidden=1
      continue
    fi
    if [[ "$seeing_forbidden" -eq 1 ]]; then
      forbidden+=("$arg")
    else
      flags+=("$arg")
    fi
  done
  local output names found
  output="$(
    cargo tree --locked --offline -p "$package" "${flags[@]}" \
      --edges normal,build --prefix none --format '{p}'
  )"
  names="$(printf '%s\n' "$output" | awk 'NF { print $1 }' | sort -u)"
  found="$(
    comm -12 <(printf '%s\n' "${forbidden[@]}" | sort -u) <(printf '%s\n' "$names")
  )"
  if [[ -n "$found" ]]; then
    fail "$package: forbidden production dependencies: $(printf '%s' "$found" | paste -sd' ' -)"
  fi
  local count
  count="$(printf '%s\n' "$names" | grep -c . || true)"
  echo "$package ${flags[*]}: dependency boundary passes ($count package names)"
}

services=(jmodguard-bot jmodguard-api jmodguard-integration-tests)
bot_stack=(serenity tokio-tungstenite tungstenite)
api_stack=(axum tower-sessions tower-sessions-core tower-sessions-memory-store)

check_tree jmodguard -- "${services[@]}" "${bot_stack[@]}" "${api_stack[@]}"
check_tree jmodguard-bot -- jmodguard-api jmodguard-integration-tests "${api_stack[@]}"
check_tree jmodguard-api -- jmodguard-bot jmodguard-integration-tests "${bot_stack[@]}"
check_tree jmodguard-api --no-default-features -- jmodguard-bot jmodguard-integration-tests "${bot_stack[@]}"
check_tree jmodguard-runtime -- "${services[@]}" "${bot_stack[@]}" "${api_stack[@]}" jmodguard
