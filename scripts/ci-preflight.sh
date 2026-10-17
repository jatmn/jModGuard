#!/usr/bin/env bash
# Shared selected GitHub Actions check groups and the full local suite.
# The remote-database job (libSQL containers) stays separate.
# Dependabot Updates dynamic workflows are not CI.
set -euo pipefail

# UTC calendar dates are YYYY-MM-DD. Expiry dates live in `.cargo/audit-expiries`.
audit_ignore_is_expired() {
  local today=$1 expiry=$2
  (
    LC_ALL=C
    [[ "$today" == "$expiry" || "$today" > "$expiry" ]]
  )
}

audit_date_is_ymd() {
  local value=$1 normalized
  [[ "$value" =~ ^[0-9]{4}-[0-9]{2}-[0-9]{2}$ ]] || return 1
  normalized="$(TZ=UTC date -u -d "${value}T00:00:00Z" +%Y-%m-%d 2>/dev/null)" || return 1
  [[ "$normalized" == "$value" ]]
}

audit_accept_today() {
  local status=$1 raw=$2
  [[ "$status" == 0 ]] || return 1
  audit_date_is_ymd "$raw" || return 1
  printf '%s\n' "$raw"
  return 0
}

audit_read_expiries() {
  local file=$1
  local line id date seen=""
  [[ -f "$file" ]] || return 1
  while IFS= read -r line || [ -n "$line" ]; do
    line="${line%%#*}"
    line="${line#"${line%%[![:space:]]*}"}"
    line="${line%"${line##*[![:space:]]}"}"
    [ -n "$line" ] || continue
    if [[ ! "$line" =~ ^([A-Z][A-Z0-9]*(-[0-9A-Z]+)+)[[:space:]]([0-9]{4}-[0-9]{2}-[0-9]{2})$ ]]; then
      return 1
    fi
    id="${BASH_REMATCH[1]}"
    date="${BASH_REMATCH[3]}"
    audit_date_is_ymd "$date" || return 1
    case " $seen " in
      *" $id "*) return 1 ;;
    esac
    seen="${seen} ${id}"
    printf '%s %s\n' "$id" "$date"
  done <"$file"
}

audit_classify_report() {
  local report=$1 ignores_out=$2
  REPORT="$report" IGNORES_OUT="$ignores_out" node --input-type=commonjs - <<'JS'
const fs = require("fs");

const idRe = /^[A-Z][A-Z0-9]*(-[0-9A-Z]+)+$/;

let report;
try {
  report = JSON.parse(fs.readFileSync(process.env.REPORT, "utf8"));
} catch {
  process.exit(1);
}

if (!report || Array.isArray(report) || typeof report !== "object") {
  process.exit(1);
}

const vulnerabilities = report.vulnerabilities;
if (
  vulnerabilities == null ||
  Array.isArray(vulnerabilities) ||
  typeof vulnerabilities !== "object"
) {
  process.exit(1);
}

if (typeof vulnerabilities.found !== "boolean") {
  process.exit(1);
}

const count = vulnerabilities.count;
if (
  !Number.isInteger(count) ||
  count < 0
) {
  process.exit(1);
}

if (!Array.isArray(vulnerabilities.list)) {
  process.exit(1);
}

for (const entry of vulnerabilities.list) {
  const advId = entry.advisory?.id;
  const pkg = entry.package;
  if (!advId || !pkg?.name || !pkg?.version) {
    process.exit(1);
  }
  console.error(`ci-preflight: advisory ${advId} ${pkg.name} ${pkg.version}`);
}

const ignoreSetting = report.settings?.ignore;
const ignores = ignoreSetting == null ? [] : ignoreSetting;
if (!Array.isArray(ignores)) {
  process.exit(1);
}

const seen = new Set();
for (const id of ignores) {
  if (typeof id !== "string" || !idRe.test(id)) {
    process.exit(1);
  }
  if (seen.has(id)) {
    process.exit(1);
  }
  seen.add(id);
}

fs.writeFileSync(process.env.IGNORES_OUT, ignores.join("\n") + (ignores.length ? "\n" : ""), "utf8");

const warnings = report.warnings || {};
for (const kind of ["yanked", "unmaintained"]) {
  for (const entry of warnings[kind] || []) {
    const pkg = entry.package;
    if (!pkg?.name || !pkg?.version) {
      process.exit(1);
    }
    const adv = entry.advisory?.id;
    if (adv) {
      console.error(
        `ci-preflight: warning ${kind} ${pkg.name} ${pkg.version} ${adv}`
      );
    } else {
      console.error(
        `ci-preflight: warning ${kind} ${pkg.name} ${pkg.version}`
      );
    }
  }
}

if (!vulnerabilities.found && count === 0) {
  process.stdout.write("-1");
} else {
  process.stdout.write(String(count));
}
JS
}

decide_audit_report() {
  local report=$1 expiries=$2 audit_rc=$3 today_rc=$4 today_raw=$5 fail_closed=$6
  local ignores_tmp count classify_rc
  local -A expiry_map=()
  local id date line ignore_count=0 expiry_count=0 restore_errexit=0
  ignores_tmp="$(mktemp)"
  case "$-" in
    *e*) restore_errexit=1 ;;
  esac
  set +e
  dar_fin() {
    if [ "$restore_errexit" -eq 1 ]; then
      set -e
    fi
  }
  count="$(audit_classify_report "$report" "$ignores_tmp")"
  classify_rc=$?
  if [ "$classify_rc" -ne 0 ]; then
    rm -f "$ignores_tmp"
    echo 'ci-preflight: cargo audit failed to run (operational error)' >&2
    dar_fin
    return 1
  fi

  while IFS= read -r line; do
    [ -n "$line" ] || continue
    id="${line%% *}"
    date="${line#* }"
    expiry_map["$id"]="$date"
    expiry_count=$((expiry_count + 1))
  done < <(audit_read_expiries "$expiries")
  local read_rc=$?
  if [ "$read_rc" -ne 0 ]; then
    rm -f "$ignores_tmp"
    echo 'ci-preflight: cargo audit expiry file is invalid' >&2
    dar_fin
    return 1
  fi

  while IFS= read -r id || [ -n "$id" ]; do
    [ -n "$id" ] || continue
    ignore_count=$((ignore_count + 1))
    date="${expiry_map[$id]:-}"
    if [ -z "$date" ]; then
      rm -f "$ignores_tmp"
      echo "ci-preflight: cargo audit ignore ${id} has no expiry in .cargo/audit-expiries" >&2
      dar_fin
      return 1
    fi
  done <"$ignores_tmp"

  for id in "${!expiry_map[@]}"; do
    if ! grep -Fxq "$id" "$ignores_tmp" 2>/dev/null; then
      rm -f "$ignores_tmp"
      echo "ci-preflight: cargo audit expiry ${id} is not ignored" >&2
      dar_fin
      return 1
    fi
  done

  if [ "$ignore_count" -gt 0 ] || [ "$expiry_count" -gt 0 ]; then
    if [ "$today_rc" -ne 0 ] || ! audit_date_is_ymd "$today_raw"; then
      rm -f "$ignores_tmp"
      echo 'ci-preflight: cargo audit exception expiry date is unavailable' >&2
      dar_fin
      return 1
    fi
    while IFS= read -r id || [ -n "$id" ]; do
      [ -n "$id" ] || continue
      date="${expiry_map[$id]}"
      if [ "$id" = RUSTSEC-2026-0258 ]; then
        echo "ci-preflight: advisory ${id} ignored (expires ${date}; see #349)" >&2
        if audit_ignore_is_expired "$today_raw" "$date"; then
          echo "ci-preflight: cargo audit exception ${id} expired on ${date}; see #349; re-review the exception" >&2
          rm -f "$ignores_tmp"
          dar_fin
          return 1
        fi
      else
        echo "ci-preflight: advisory ${id} ignored (expires ${date})" >&2
        if audit_ignore_is_expired "$today_raw" "$date"; then
          echo "ci-preflight: cargo audit exception ${id} expired on ${date}; re-review the exception" >&2
          rm -f "$ignores_tmp"
          dar_fin
          return 1
        fi
      fi
    done <"$ignores_tmp"
  fi

  rm -f "$ignores_tmp"

  if [ "$count" = -1 ]; then
    if [ "$audit_rc" -eq 0 ]; then
      dar_fin
      return 0
    fi
    echo 'ci-preflight: cargo audit failed to run (unexpected exit 1 without advisories)' >&2
    dar_fin
    return 1
  fi

  if [ "$fail_closed" = 1 ]; then
    echo "ci-preflight: cargo audit failed closed (scheduled CI; ${count} advisories)" >&2
    dar_fin
    return 1
  fi

  echo "ci-preflight: cargo audit reported ${count} advisories; pull requests and ordinary main pushes do not fail the supply-chain check" >&2
  dar_fin
  return 0
}

if [ "${1:-}" = "--audit-expiry-self-check" ]; then
  fail() { echo "ci-preflight: audit expiry self-check: $*" >&2; exit 1; }
  self_root="$(cd "$(dirname "$0")/.." && pwd)"
  real_expiries="${self_root}/.cargo/audit-expiries"

  audit_ignore_is_expired 2024-05-01 2024-05-02 && fail "2024-05-01 must stay valid before 2024-05-02"
  audit_ignore_is_expired 2024-05-02 2024-05-02 || fail "2024-05-02 must expire on 2024-05-02"
  audit_ignore_is_expired 2024-05-03 2024-05-02 || fail "2024-05-03 must expire after 2024-05-02"

  audit_accept_today 1 2024-05-02 >/dev/null 2>&1 && fail "audit_accept_today must fail for status 1"
  audit_accept_today 0 not-a-date >/dev/null 2>&1 && fail "audit_accept_today must fail for not-a-date"
  audit_accept_today 0 2027-13-40 >/dev/null 2>&1 && fail "audit_accept_today must fail for 2027-13-40"
  out="$(audit_accept_today 0 2024-05-02)" || fail "audit_accept_today must accept 2024-05-02"
  [ "$out" = 2024-05-02 ] || fail "audit_accept_today must echo 2024-05-02"

  tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' EXIT

  write_report() {
    printf '%s' "$1" >"$tmp/report.json"
  }

  decide_expect() {
    local expect_rc=$1
    local report_json=$2 expiries_file=$3 audit_rc=$4 today_rc=$5 today_raw=$6 fail_closed=$7
    local rc
    write_report "$report_json"
    : >"$tmp/decide.err"
    set +e
    decide_audit_report "$tmp/report.json" "$expiries_file" "$audit_rc" "$today_rc" "$today_raw" "$fail_closed" 2>"$tmp/decide.err"
    rc=$?
    set -e
    if [ "$rc" -ne "$expect_rc" ]; then
      cat "$tmp/decide.err" >&2
      fail "decide_audit_report expected rc ${expect_rc}, got ${rc}"
    fi
  }

  printf '\n' >"$tmp/empty-expiries"

  decide_expect 1 '{' "$tmp/empty-expiries" 0 0 2024-05-02 0
  grep -Fq 'operational error' "$tmp/decide.err" || fail 'malformed JSON must print operational error'

  decide_expect 1 '{}' "$tmp/empty-expiries" 0 0 2024-05-02 0
  grep -Fq 'operational error' "$tmp/decide.err" || fail 'missing vulnerabilities must print operational error'

  decide_expect 1 '{"vulnerabilities":null}' "$tmp/empty-expiries" 0 0 2024-05-02 0
  grep -Fq 'operational error' "$tmp/decide.err" || fail 'null vulnerabilities must print operational error'

  decide_expect 1 '{"vulnerabilities":[]}' "$tmp/empty-expiries" 0 0 2024-05-02 0
  grep -Fq 'operational error' "$tmp/decide.err" || fail 'array vulnerabilities must print operational error'

  decide_expect 0 '{"vulnerabilities":{"found":false,"count":0,"list":[]}}' "$tmp/empty-expiries" 0 0 2024-05-02 0
  decide_expect 0 '{"vulnerabilities":{"found":false,"count":0,"list":[]}}' "$tmp/empty-expiries" 0 0 2024-05-02 1

  one_adv='{"vulnerabilities":{"found":true,"count":1,"list":[{"advisory":{"id":"RUSTSEC-2020-0001"},"package":{"name":"crate","version":"1.0.0"}}]},"settings":{"ignore":[]}}'
  decide_expect 0 "$one_adv" "$tmp/empty-expiries" 0 0 2024-05-02 0
  grep -Fq 'pull requests and ordinary main pushes do not fail' "$tmp/decide.err" || fail 'one advisory fail-open must print PR sentence'
  decide_expect 1 "$one_adv" "$tmp/empty-expiries" 0 0 2024-05-02 1
  grep -Fq 'failed closed' "$tmp/decide.err" || fail 'one advisory fail-closed must print failed closed'

  decide_expect 1 '{"vulnerabilities":{"found":false,"count":0,"list":[]}}' "$tmp/empty-expiries" 1 0 2024-05-02 0
  grep -Fq 'unexpected exit 1 without advisories' "$tmp/decide.err" || fail 'exit 1 empty report must print unexpected exit 1'

  printf 'RUSTSEC-2026-0001 2099-01-01\n' >"$tmp/partial-expiries"
  two_ignore='{"vulnerabilities":{"found":false,"count":0,"list":[]},"settings":{"ignore":["RUSTSEC-2026-0001","RUSTSEC-2026-0002"]}}'
  decide_expect 1 "$two_ignore" "$tmp/partial-expiries" 0 0 2024-05-02 0
  grep -Fq 'RUSTSEC-2026-0002' "$tmp/decide.err" || fail 'missing expiry must name RUSTSEC-2026-0002'
  grep -Fq 'has no expiry' "$tmp/decide.err" || fail 'missing expiry must print has no expiry'

  printf 'RUSTSEC-2026-9999 2099-01-01\n' >"$tmp/orphan-expiry"
  no_ignore='{"vulnerabilities":{"found":false,"count":0,"list":[]},"settings":{"ignore":[]}}'
  decide_expect 1 "$no_ignore" "$tmp/orphan-expiry" 0 0 2024-05-02 0
  grep -Fq 'is not ignored' "$tmp/decide.err" || fail 'orphan expiry must print is not ignored'

  printf 'RUSTSEC-2026-0001 2099-06-01\nRUSTSEC-2026-0002 2099-12-01\n' >"$tmp/two-future"
  two_ignore_ok='{"vulnerabilities":{"found":false,"count":0,"list":[]},"settings":{"ignore":["RUSTSEC-2026-0001","RUSTSEC-2026-0002"]}}'
  decide_expect 0 "$two_ignore_ok" "$tmp/two-future" 0 0 2024-05-02 0
  grep -Fq 'RUSTSEC-2026-0001 ignored (expires 2099-06-01)' "$tmp/decide.err" || fail 'two future: first ignore line'
  grep -Fq 'RUSTSEC-2026-0002 ignored (expires 2099-12-01)' "$tmp/decide.err" || fail 'two future: second ignore line'

  printf 'RUSTSEC-2026-0001 2024-05-01\nRUSTSEC-2026-0002 2099-12-01\n' >"$tmp/today-expire"
  decide_expect 1 "$two_ignore_ok" "$tmp/today-expire" 0 0 2024-05-02 0
  grep -Fq 'expired on' "$tmp/decide.err" || fail 'today equal earlier expiry must print expired on'
  grep -Fq 'RUSTSEC-2026-0001' "$tmp/decide.err" || fail 'today equal earlier expiry must name id'
  grep -Fq '2024-05-01' "$tmp/decide.err" || fail 'today equal earlier expiry must name date'

  printf 'RUSTSEC-2026-0001 2099-01-01\n' >"$tmp/one-future"
  one_ignore='{"vulnerabilities":{"found":false,"count":0,"list":[]},"settings":{"ignore":["RUSTSEC-2026-0001"]}}'
  decide_expect 1 "$one_ignore" "$tmp/one-future" 0 1 '' 0
  grep -Fq 'expiry date is unavailable' "$tmp/decide.err" || fail 'today_rc 1 must print expiry date is unavailable'
  decide_expect 1 "$one_ignore" "$tmp/one-future" 0 0 'yesterday' 0
  grep -Fq 'expiry date is unavailable' "$tmp/decide.err" || fail 'non-calendar today must print expiry date is unavailable'

  decide_expect 0 '{"vulnerabilities":{"found":false,"count":0,"list":[]}}' "$tmp/empty-expiries" 0 1 '' 0

  rustsec_expiry="$(awk '/^RUSTSEC-2026-0258 / {print $2; exit}' "$real_expiries")"
  [ -n "$rustsec_expiry" ] || fail 'real audit-expiries must list RUSTSEC-2026-0258'
  day_before="$(TZ=UTC date -u -d "${rustsec_expiry}T00:00:00Z - 1 day" +%Y-%m-%d)"
  rustsec_ok='{"vulnerabilities":{"found":false,"count":0,"list":[]},"settings":{"ignore":["RUSTSEC-2026-0258"]}}'
  decide_expect 0 "$rustsec_ok" "$real_expiries" 0 0 "$day_before" 0
  grep -Fq "ignored (expires ${rustsec_expiry}; see #349)" "$tmp/decide.err" || fail 'documented date: day before must print ignored with see #349'
  decide_expect 1 "$rustsec_ok" "$real_expiries" 0 0 "$rustsec_expiry" 0
  grep -Fq "expired on ${rustsec_expiry}; see #349; re-review the exception" "$tmp/decide.err" || fail 'documented date: expiry day must print expired with see #349'

  echo "ci-preflight: audit expiry self-check passed"
  exit 0
fi

root="$(git rev-parse --show-toplevel)"
cd "$root"

if [ -z "${JMODGUARD_CI_PREFIX:-}" ]; then
  if [ -n "${HOME:-}" ]; then
    JMODGUARD_CI_PREFIX="$HOME/.cache/jmodguard-ci"
  else
    JMODGUARD_CI_PREFIX=/opt/jmodguard-ci
  fi
fi
# shellcheck source=scripts/ci-ensure-cargo-tool.sh
source "$root/scripts/ci-ensure-cargo-tool.sh"
# shellcheck source=scripts/ci-run-timed-cargo-test.sh
source "$root/scripts/ci-run-timed-cargo-test.sh"

usage() {
  cat <<'EOF'
Usage: bash scripts/ci-preflight.sh [--base <git-revision>] [--only <group>]

Run the CI gates locally or on an ephemeral GitHub-hosted Linux runner.
With no --only, run the full suite (local hooks and a complete check).
GitHub Actions groups affected checks into jobs. Known groups: contracts,
frontend, fmt, dylint, api, bot, runtime, updater, core, integration,
boundaries, publish-tests, bootstrap, pullfrog, supply-chain.
By default the whitespace diff base is origin/main. Pass --base origin/<branch>
for a PR whose base is not main. Scheduled builds set
JMODGUARD_AUDIT_FAIL_CLOSED=1 so cargo-audit fails the job.
EOF
}

base_ref="${JMODGUARD_PREFLIGHT_BASE:-origin/main}"
only_names=""
known_only="contracts frontend fmt dylint api bot runtime updater core integration boundaries publish-tests bootstrap pullfrog supply-chain"
while [ "$#" -gt 0 ]; do
  case "$1" in
    --base)
      [ "$#" -ge 2 ] || { usage >&2; exit 2; }
      base_ref="$2"
      shift 2
      ;;
    --only)
      [ "$#" -ge 2 ] || { usage >&2; exit 2; }
      case " ${known_only} " in
        *" $2 "*) ;;
        *) echo "ci-preflight: unknown --only group: $2" >&2; exit 2 ;;
      esac
      only_names="${only_names} $2"
      shift 2
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      usage >&2
      exit 2
      ;;
  esac
done

should_run() {
  if [ -z "${only_names}" ]; then
    return 0
  fi
  case " ${only_names} " in
    *" $1 "*) return 0 ;;
    *) return 1 ;;
  esac
}

if should_run contracts; then
  node scripts/ci-workflow-test.mjs
fi

if should_run contracts; then
for hook in pre-commit pre-push; do
  hook_path="$root/.githooks/$hook"
  if [ ! -f "$hook_path" ]; then
    continue
  fi
  if [ ! -x "$hook_path" ]; then
    echo "ci-preflight: .githooks/$hook must be executable (git skips non-executable hooks)" >&2
    exit 1
  fi
  if ! grep -Fq 'scripts/ci-preflight.sh' "$hook_path"; then
    echo "ci-preflight: .githooks/$hook must invoke scripts/ci-preflight.sh" >&2
    exit 1
  fi
done
fi

require_command() {
  local command="$1"
  local install="$2"
  if ! command -v "$command" >/dev/null 2>&1; then
    echo "ci-preflight: missing $command; install it with: $install" >&2
    exit 2
  fi
}

if should_run fmt; then
  require_command rustfmt 'rustup component add rustfmt'
fi

run_timed_cargo_test() {
  if [ "${1:-}" != cargo ] || [ "${2:-}" != test ]; then
    echo 'ci-preflight: run_timed_cargo_test expects cargo test' >&2
    exit 1
  fi
  shift 2

  local cargo_args=()
  local harness=()
  local past_dd=0 arg
  for arg in "$@"; do
    if [ "$past_dd" -eq 1 ]; then
      harness+=("$arg")
      continue
    fi
    if [ "$arg" = -- ]; then
      past_dd=1
      continue
    fi
    cargo_args+=("$arg")
  done

  local nextest_help nextest_ok=0 cfg
  if command -v cargo-nextest >/dev/null 2>&1; then
    nextest_help="$(cargo nextest run --help 2>&1 || true)"
    if printf '%s\n' "$nextest_help" | grep -Fq -- '--config-file'; then
      nextest_ok=1
    fi
  fi

  local can_nextest=0
  if [ "$nextest_ok" -eq 1 ]; then
    local bad_harness=0 h locked=0 workspace=0 has_pkg=0
    for h in "${harness[@]}"; do
      case "$h" in
        --include-ignored|--ignored|--nocapture) ;;
        *) bad_harness=1 ;;
      esac
    done
    if [ "$bad_harness" -eq 0 ]; then
      local i=0 need_val=0
      while [ "$i" -lt "${#cargo_args[@]}" ]; do
        arg="${cargo_args[$i]}"
        if [ "$need_val" -eq 1 ]; then
          need_val=0
          i=$((i + 1))
          continue
        fi
        case "$arg" in
          --locked) locked=1 ;;
          --workspace) workspace=1 ;;
          --no-default-features|--lib) ;;
          -p|--test)
            need_val=1
            [ "$arg" = -p ] && has_pkg=1
            ;;
          *)
            bad_harness=1
            ;;
        esac
        i=$((i + 1))
      done
      [ "$need_val" -eq 1 ] && bad_harness=1
      if [ "$bad_harness" -eq 0 ] && [ "$locked" -eq 1 ] && { [ "$workspace" -eq 1 ] || [ "$has_pkg" -eq 1 ]; }; then
        can_nextest=1
      fi
    fi
  fi

  if [ "$can_nextest" -eq 1 ]; then
    local nextest_args=(nextest run --locked --profile default)
    cfg="$(mktemp)"
    cat >"$cfg" <<'EOF'
[profile.default.slow-timeout]
period = "60s"
terminate-after = 3
EOF
    nextest_args+=(--config-file "$cfg" --failure-output immediate)
    local i=0
    while [ "$i" -lt "${#cargo_args[@]}" ]; do
      arg="${cargo_args[$i]}"
      case "$arg" in
        --locked) ;;
        --workspace) nextest_args+=(--workspace) ;;
        --no-default-features) nextest_args+=(--no-default-features) ;;
        -p)
          i=$((i + 1))
          nextest_args+=(-p "${cargo_args[$i]}")
          ;;
        --lib) nextest_args+=(--lib) ;;
        --test)
          i=$((i + 1))
          nextest_args+=(--test "${cargo_args[$i]}")
          ;;
        *) nextest_args+=("$arg") ;;
      esac
      i=$((i + 1))
    done
    local h
    for h in "${harness[@]}"; do
      case "$h" in
        --include-ignored)
          if printf '%s\n' "$nextest_help" | grep -Fq -- '--run-ignored'; then
            nextest_args+=(--run-ignored all)
          else
            can_nextest=0
          fi
          ;;
        --nocapture)
          if printf '%s\n' "$nextest_help" | grep -Fq -- '--no-capture'; then
            nextest_args+=(--no-capture)
          else
            can_nextest=0
          fi
          ;;
        --ignored) can_nextest=0 ;;
      esac
    done
    if [ "$can_nextest" -eq 1 ]; then
      echo 'ci-preflight: per-test timeout via cargo-nextest slow-timeout period 60s terminate-after 3; doctests use GNU timeout 180s'
      set +e
      cargo "${nextest_args[@]}"
      local nextest_rc=$?
      set -e
      rm -f "$cfg"
      [ "$nextest_rc" -eq 0 ] || return "$nextest_rc"
      run_timed_cargo_test_doctests "${cargo_args[@]}"
      return $?
    fi
    rm -f "$cfg"
    echo 'ci-preflight: cargo-nextest cannot express this cargo test command; using GNU timeout per test'
  fi

  if ! command -v timeout >/dev/null 2>&1; then
    echo 'ci-preflight: GNU timeout is not installed; running without a per-test timeout'
    if [ "${#harness[@]}" -gt 0 ]; then
      cargo test "${cargo_args[@]}" -- "${harness[@]}"
    else
      cargo test "${cargo_args[@]}"
    fi
    return $?
  fi

  echo 'ci-preflight: per-test timeout 180s via GNU timeout'
  run_timed_cargo_test_gnu "${cargo_args[@]}" -- "${harness[@]}"
}

# cargo-audit 0.22.2 uses exit 1 for both advisory reports and some operational
# failures (for example advisory DB fetch/load). Classify from JSON stdout and
# stderr instead of trusting exit 1 alone.
run_cargo_audit_gate() {
  local audit_out audit_err audit_rc today_raw today_rc fail_closed decide_rc
  audit_out="$(mktemp)"
  audit_err="$(mktemp)"
  set +e
  cargo audit --format json >"$audit_out" 2>"$audit_err"
  audit_rc=$?
  set -e
  if [ -s "$audit_err" ]; then
    cat "$audit_err" >&2
  fi

  if [ "$audit_rc" -eq 2 ]; then
    echo "ci-preflight: cargo audit failed to run (exit ${audit_rc})" >&2
    rm -f "$audit_out" "$audit_err"
    return 1
  fi

  if [ "$audit_rc" -ne 0 ] && [ "$audit_rc" -ne 1 ]; then
    echo "ci-preflight: cargo audit failed to run (exit ${audit_rc})" >&2
    rm -f "$audit_out" "$audit_err"
    return 1
  fi

  if [ "$audit_rc" -eq 1 ]; then
    if grep -qE '^error:' "$audit_err" || [ ! -s "$audit_out" ]; then
      echo 'ci-preflight: cargo audit failed to run (operational error)' >&2
      rm -f "$audit_out" "$audit_err"
      return 1
    fi
  fi

  today_raw=""
  today_rc=1
  if today_raw="$(date -u +%Y-%m-%d 2>/dev/null)"; then
    today_rc=0
  fi

  fail_closed=0
  if [ "${JMODGUARD_AUDIT_FAIL_CLOSED:-}" = "1" ]; then
    fail_closed=1
  fi

  set +e
  decide_audit_report "$audit_out" "$root/.cargo/audit-expiries" "$audit_rc" "$today_rc" "$today_raw" "$fail_closed"
  decide_rc=$?
  set -e
  rm -f "$audit_out" "$audit_err"
  return "$decide_rc"
}

whitespace_check() {
  local merge_base
  if [[ "${JMODGUARD_DIFF_BASE:-}" =~ ^[0-9a-f]{40}$ && "${JMODGUARD_DIFF_HEAD:-}" =~ ^[0-9a-f]{40}$ ]] \
      && git cat-file -e "${JMODGUARD_DIFF_BASE}^{commit}" 2>/dev/null \
      && git cat-file -e "${JMODGUARD_DIFF_HEAD}^{commit}" 2>/dev/null; then
    local range_base="$JMODGUARD_DIFF_BASE"
    if [ "${GITHUB_EVENT_NAME:-}" = pull_request ]; then
      range_base="$(git merge-base "$range_base" "$JMODGUARD_DIFF_HEAD")"
    fi
    git diff --check "$range_base" "$JMODGUARD_DIFF_HEAD"
    return 0
  fi
  if git rev-parse --verify --quiet "${base_ref}^{commit}" >/dev/null; then
    if ! merge_base="$(git merge-base "$base_ref" HEAD)"; then
      merge_base="$base_ref"
    fi
    # Main checkouts have origin/main == HEAD, so merge-base vs the
    # working tree is empty. Compare the first-parent patch in that case.
    if [ "$(git rev-parse "$merge_base")" = "$(git rev-parse HEAD)" ]; then
      if git rev-parse --verify --quiet HEAD^ >/dev/null; then
        echo "ci-preflight: git diff --check HEAD^ HEAD (base $base_ref is HEAD)"
        git diff --check HEAD^ HEAD
        return 0
      fi
    fi
    echo "ci-preflight: git diff --check $merge_base"
    git diff --check "$merge_base"
    return 0
  fi
  echo "ci-preflight: git diff --check (working tree; base $base_ref not found)"
  git diff --check
}

run_cargo() {
  if should_run fmt || should_run dylint || should_run api || should_run bot || should_run runtime || should_run updater || should_run core || should_run integration || should_run boundaries || should_run supply-chain; then
    return 0
  fi
  return 1
}

# Rust's default embed-ui and the publication tests consume the same built assets.
if should_run api || should_run integration || should_run dylint || should_run frontend || should_run publish-tests; then
  bash scripts/require-host-eslint-node.sh
  npm ci --ignore-scripts --prefix tools/webui
  npm audit --prefix tools/webui --audit-level=high
  npm run build --prefix tools/webui
fi

if run_cargo; then
  echo 'ci-preflight: cargo update --workspace --locked'
  cargo update --workspace --locked
fi

if should_run fmt; then
  echo 'ci-preflight: cargo fmt --check'
  cargo fmt --check
fi

if should_run dylint; then
  echo 'ci-preflight: bash scripts/dylint-test.sh'
  bash scripts/dylint-test.sh
  echo 'ci-preflight: bash scripts/dylint.sh'
  if [ -z "${only_names}" ]; then
    # Full local gates must not inherit an affected-only CI scope.
    env JMODGUARD_DYLINT_PACKAGES= bash scripts/dylint.sh
  else
    bash scripts/dylint.sh
  fi
fi

if should_run contracts; then
  whitespace_check
  echo 'ci-preflight: bash scripts/select-checks-test.sh'
  bash scripts/select-checks-test.sh

  echo 'ci-preflight: bash scripts/ci-timed-cargo-test.sh'
  bash scripts/ci-timed-cargo-test.sh
fi

if should_run publish-tests; then
  echo 'ci-preflight: bash scripts/publish-image-test.sh'
  bash scripts/publish-image-test.sh
  bash scripts/prepare-publication-test.sh
fi

if should_run bootstrap; then
  echo 'ci-preflight: bash scripts/bootstrap-container-test.sh'
  bash scripts/bootstrap-container-test.sh
fi

if [ -z "${only_names}" ]; then
  echo 'ci-preflight: cargo clippy --locked --workspace --all-targets -- -D warnings'
  cargo clippy --locked --workspace --all-targets -- -D warnings

  echo 'ci-preflight: cargo clippy --locked -p jmodguard-api --no-default-features --all-targets -- -D warnings'
  cargo clippy --locked -p jmodguard-api --no-default-features --all-targets -- -D warnings

  echo 'ci-preflight: cargo test --locked --workspace'
  run_timed_cargo_test cargo test --locked --workspace || exit $?

  echo 'ci-preflight: cargo test --locked -p jmodguard-api --no-default-features --lib'
  run_timed_cargo_test cargo test --locked -p jmodguard-api --no-default-features --lib || exit $?
else
  if should_run api; then
    echo 'ci-preflight: cargo clippy --locked -p jmodguard-api --all-targets -- -D warnings'
    cargo clippy --locked -p jmodguard-api --all-targets -- -D warnings
    echo 'ci-preflight: cargo clippy --locked -p jmodguard-api --no-default-features --all-targets -- -D warnings'
    cargo clippy --locked -p jmodguard-api --no-default-features --all-targets -- -D warnings
    echo 'ci-preflight: cargo test --locked -p jmodguard-api'
    run_timed_cargo_test cargo test --locked -p jmodguard-api || exit $?
    echo 'ci-preflight: cargo test --locked -p jmodguard-api --no-default-features --lib'
    run_timed_cargo_test cargo test --locked -p jmodguard-api --no-default-features --lib || exit $?
    echo 'ci-preflight: cargo build --locked -p jmodguard-api'
    cargo build --locked -p jmodguard-api
    echo 'ci-preflight: cargo build --locked -p jmodguard-api --no-default-features'
    cargo build --locked -p jmodguard-api --no-default-features
  fi
  if should_run bot; then
    echo 'ci-preflight: cargo clippy --locked -p jmodguard-bot --all-targets -- -D warnings'
    cargo clippy --locked -p jmodguard-bot --all-targets -- -D warnings
    echo 'ci-preflight: cargo test --locked -p jmodguard-bot'
    run_timed_cargo_test cargo test --locked -p jmodguard-bot || exit $?
    echo 'ci-preflight: cargo build --locked -p jmodguard-bot'
    cargo build --locked -p jmodguard-bot
  fi
  if should_run runtime; then
    echo 'ci-preflight: cargo clippy --locked -p jmodguard-runtime --all-targets -- -D warnings'
    cargo clippy --locked -p jmodguard-runtime --all-targets -- -D warnings
    echo 'ci-preflight: cargo test --locked -p jmodguard-runtime'
    run_timed_cargo_test cargo test --locked -p jmodguard-runtime || exit $?
  fi
  if should_run updater; then
    echo 'ci-preflight: cargo clippy --locked -p jmodguard-updater --all-targets -- -D warnings'
    cargo clippy --locked -p jmodguard-updater --all-targets -- -D warnings
    echo 'ci-preflight: cargo test --locked -p jmodguard-updater'
    run_timed_cargo_test cargo test --locked -p jmodguard-updater || exit $?
  fi
  if should_run core; then
    echo 'ci-preflight: cargo clippy --locked -p jmodguard --all-targets -- -D warnings'
    cargo clippy --locked -p jmodguard --all-targets -- -D warnings
    echo 'ci-preflight: cargo test --locked -p jmodguard'
    run_timed_cargo_test cargo test --locked -p jmodguard || exit $?
  fi
  if should_run integration; then
    echo 'ci-preflight: cargo clippy --locked -p jmodguard-integration-tests --all-targets -- -D warnings'
    cargo clippy --locked -p jmodguard-integration-tests --all-targets -- -D warnings
    echo 'ci-preflight: cargo test --locked -p jmodguard-integration-tests'
    run_timed_cargo_test cargo test --locked -p jmodguard-integration-tests || exit $?
    echo 'ci-preflight: cargo build --locked -p jmodguard-bot'
    cargo build --locked -p jmodguard-bot
    echo 'ci-preflight: cargo build --locked -p jmodguard-api'
    cargo build --locked -p jmodguard-api
    echo 'ci-preflight: cargo test --locked -p jmodguard-integration-tests --test dual_process_smoke -- --include-ignored --nocapture'
    run_timed_cargo_test cargo test --locked -p jmodguard-integration-tests --test dual_process_smoke -- --include-ignored --nocapture || exit $?
  fi
fi

if should_run boundaries; then
  echo 'ci-preflight: bash scripts/check-service-boundaries.sh'
  bash scripts/check-service-boundaries.sh
fi

if should_run contracts; then
  echo 'ci-preflight: bash scripts/public-install-acceptance-test.sh'
  bash scripts/public-install-acceptance-test.sh

  echo 'ci-preflight: bash scripts/dependabot-config-test.sh'
  bash scripts/dependabot-config-test.sh
fi

if should_run pullfrog; then
  echo 'ci-preflight: node --test scripts/pullfrog-command-check.test.mjs'
  node --test scripts/pullfrog-command-check.test.mjs
fi

if should_run frontend; then
  echo 'ci-preflight: bash scripts/require-host-eslint-node.sh'
  bash scripts/require-host-eslint-node.sh

  echo 'ci-preflight: npm ci --ignore-scripts --prefix tools/eslint'
  npm ci --ignore-scripts --prefix tools/eslint

  echo 'ci-preflight: npm audit --prefix tools/eslint --audit-level=high'
  npm audit --prefix tools/eslint --audit-level=high

  echo 'ci-preflight: bash tools/eslint-web-selftest.sh'
  bash tools/eslint-web-selftest.sh

  echo 'ci-preflight: node --test scripts/frontend-state-test.mjs'
  node --test scripts/frontend-state-test.mjs

  echo 'ci-preflight: node --test scripts/frontend-contrast-test.mjs'
  node --test scripts/frontend-contrast-test.mjs

  echo 'ci-preflight: node --test scripts/frontend-warmup-test.mjs'
  node --test scripts/frontend-warmup-test.mjs

  echo 'ci-preflight: node --test scripts/publish-frontend-test.mjs'
  node --test scripts/publish-frontend-test.mjs

  echo 'ci-preflight: node --test scripts/package-webui-test.mjs'
  node --test scripts/package-webui-test.mjs

  echo 'ci-preflight: node --test scripts/administration-state-test.mjs'
  node --test scripts/administration-state-test.mjs
fi

if [ -z "${only_names}" ]; then
  echo 'ci-preflight: cargo build --locked -p jmodguard-bot'
  cargo build --locked -p jmodguard-bot

  echo 'ci-preflight: cargo build --locked -p jmodguard-api'
  cargo build --locked -p jmodguard-api

  echo 'ci-preflight: cargo test --locked -p jmodguard-integration-tests --test dual_process_smoke -- --include-ignored --nocapture'
  run_timed_cargo_test cargo test --locked -p jmodguard-integration-tests --test dual_process_smoke -- --include-ignored --nocapture || exit $?

  echo 'ci-preflight: cargo build --locked -p jmodguard-api --no-default-features'
  cargo build --locked -p jmodguard-api --no-default-features
fi

if should_run supply-chain; then
  echo 'ci-preflight: bash scripts/ci-preflight-test.sh'
  bash scripts/ci-preflight-test.sh

  ensure_cargo_tool cargo-deny cargo-deny 0.20.2
  ensure_cargo_tool cargo-audit cargo-audit 0.22.2

  echo 'ci-preflight: cargo deny check bans licenses sources'
  cargo deny check bans licenses sources

  echo 'ci-preflight: cargo audit'
  run_cargo_audit_gate
fi

echo 'ci-preflight: CI test gates passed'
