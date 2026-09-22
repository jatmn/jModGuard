#!/usr/bin/env bash
# Public-source install docs must not require the private ops repository.
set -euo pipefail

root="$(git rev-parse --show-toplevel 2>/dev/null || true)"
if [[ -z "$root" ]]; then
  root="$(cd "$(dirname "$0")/.." && pwd)"
fi
cd "$root"

fail() {
  echo "public-install-acceptance-test: $*" >&2
  exit 1
}

require_contains() {
  local path="$1"
  local needle="$2"
  grep -Fq -- "$needle" "$root/$path" || fail "missing in $path: $needle"
}

require_absent() {
  local path="$1"
  local needle="$2"
  if grep -Fq -- "$needle" "$root/$path"; then
    fail "forbidden in $path: $needle"
  fi
}

# test_license_is_polyform_noncommercial
require_contains LICENSE.md "PolyForm Noncommercial License 1.0.0"
require_contains Cargo.toml "LicenseRef-PolyForm-Noncommercial-1.0.0"

# test_install_guide_is_public_and_lists_deferred_checks
require_contains docs/INSTALL.md "without the private"
require_contains docs/INSTALL.md "ops repo"
require_contains docs/INSTALL.md "not required"
require_contains docs/INSTALL.md "Still-deferred live provider checks"
require_contains docs/INSTALL.md "Live Bunny Database"
require_contains docs/INSTALL.md "recover-update"
require_contains docs/INSTALL.md "LICENSE.md"

# test_aws_path_does_not_promise_rds
require_contains docs/DEPLOY-AWS.md "There is **no** RDS"
require_contains docs/DEPLOY-AWS.md "Not RDS/Postgres"
require_contains docs/DEPLOY-AWS.md "EC2"
require_contains docs/DEPLOY-AWS.md "libSQL"
if grep -Fiq "create an RDS instance" "$root/docs/DEPLOY-AWS.md"; then
  fail "forbidden in docs/DEPLOY-AWS.md: create an RDS instance"
fi
require_absent docs/DEPLOY-AWS.md 'jmodguard-bot`, `jmodguard-api`, `jmodguard-conductor`'

# test_public_artifact_wording_matches_install
require_contains docs/INSTALL.md "does not upload a separate frontend"
require_absent docs/DEPLOY-BUNNY.md "Buildkite frontend artifact"
require_absent docs/WEBUI.md "matching Buildkite artifact"
require_contains docs/WEBUI.md "does not upload a separate frontend"
require_absent docs/OVERHAUL.md "or Buildkite artifacts is the required path"
require_contains docs/OVERHAUL.md "does not upload a separate frontend"
require_absent crates/conductor/src/install.rs "Buildkite already publishes public bot/API/conductor/frontend artifacts"
require_contains crates/conductor/src/install.rs "Nightly/Stable/manual"

# test_bunny_and_webui_do_not_require_private_ops
require_absent docs/DEPLOY-BUNNY.md "The UI files are published by **GitHub Actions in your private ops repo**."
require_absent docs/DEPLOY-BUNNY.md "examples/private-ops"
if ! grep -Fiq "not required" "$root/docs/DEPLOY-BUNNY.md"; then
  fail "missing in docs/DEPLOY-BUNNY.md: not required"
fi
if ! grep -Fiq "not required" "$root/docs/WEBUI.md"; then
  fail "missing in docs/WEBUI.md: not required"
fi
require_absent docs/WEBUI.md "examples/private-ops"

# test_bunny_publish_script_env_names_in_operator_docs
require_contains docs/DEPLOY-BUNNY.md "BUNNY_STORAGE_ZONE"
require_contains docs/DEPLOY-BUNNY.md "BUNNY_STORAGE_PASSWORD"
require_contains docs/DEPLOY-BUNNY.md "BUNNY_PULL_ZONE_ID"
require_contains docs/DEPLOY-BUNNY.md "BUNNY_API_KEY"
require_contains docs/WEBUI.md "BUNNY_STORAGE_ZONE"
require_contains docs/WEBUI.md "BUNNY_PULL_ZONE_ID"

# test_bunny_dashboard_copy_matches_live_controls
for relative in docs/DEPLOY-BUNNY.md docs/WEBUI.md docs/UPGRADE.md docs/SETUP.md; do
  require_absent "$relative" "Change Origin URL"
  require_absent "$relative" "Disable Cookies"
done
require_contains docs/DEPLOY-BUNNY.md "Override Origin URL"
require_contains docs/WEBUI.md "Override Origin URL"
require_contains docs/UPGRADE.md "Override Origin URL"
require_contains docs/DEPLOY-BUNNY.md "Strip Response Cookies"
require_contains docs/WEBUI.md "Strip Response Cookies"
require_contains docs/DEPLOY-BUNNY.md "at least one replication region"
require_contains docs/WEBUI.md "at least one replication region"
require_contains docs/DEPLOY-BUNNY.md "Owned domain or subdomain"
require_contains docs/WEBUI.md "owned domain/subdomain"
require_contains docs/DEPLOY-BUNNY.md "does **not** expose sticky-session settings"
require_contains docs/WEBUI.md "not on the add-endpoint screen"
require_contains docs/DEPLOY-BUNNY.md "Open the **public admin host**"
require_contains docs/DEPLOY-BUNNY.md "a browser on the **public admin host**"
require_contains docs/DEPLOY-BUNNY.md "is the **public admin host**"
require_absent docs/DEPLOY-BUNNY.md "Open the **CDN** hostname"
require_absent docs/DEPLOY-BUNNY.md "is the **CDN** origin"

# test_discord_invite_permissions_follow_url_generator_order
node --input-type=module - "$root" <<'JS' || fail "discord invite permission order"
import fs from "node:fs";
import path from "node:path";

const root = process.argv[2];
const expected = [
  "View Audit Log",
  "Kick Members",
  "Ban Members",
  "Manage Nicknames",
  "Manage Roles",
  "Moderate Members",
  "View Channels",
  "Send Messages",
  "Manage Messages",
  "Read Message History",
];
const setup = fs.readFileSync(path.join(root, "docs/SETUP.md"), "utf8");
const start = setup.indexOf("Tick them in this Discord URL Generator order");
const end = setup.indexOf("Open the generated URL", start);
if (start < 0 || end < 0) {
  process.stderr.write("docs/SETUP.md missing Discord URL Generator permission checklist\n");
  process.exit(1);
}
const names = [...setup.slice(start, end).matchAll(/- `([^`]+)`/g)].map((match) => match[1]);
if (JSON.stringify(names) !== JSON.stringify(expected)) {
  process.stderr.write(
    `docs/SETUP.md permission order ${JSON.stringify(names)} != ${JSON.stringify(expected)}\n`,
  );
  process.exit(1);
}
const bunny = fs.readFileSync(path.join(root, "docs/DEPLOY-BUNNY.md"), "utf8");
const idx = bunny.indexOf("**General:**");
const idx2 = bunny.indexOf("**Text:**", idx);
const idx3 = bunny.indexOf("## 7. Acceptance test", idx2);
if (Math.min(idx, idx2, idx3) < 0) {
  process.stderr.write("docs/DEPLOY-BUNNY.md missing Discord URL Generator permission lists\n");
  process.exit(1);
}
const chunk = bunny.slice(idx, idx3);
const positions = [];
for (const name of expected) {
  const pos = chunk.indexOf(name);
  if (pos < 0) {
    process.stderr.write(`docs/DEPLOY-BUNNY.md missing permission ${name}\n`);
    process.exit(1);
  }
  positions.push(pos);
}
const sorted = [...positions].sort((a, b) => a - b);
if (JSON.stringify(positions) !== JSON.stringify(sorted)) {
  process.stderr.write("docs/DEPLOY-BUNNY.md permissions are not in URL Generator order\n");
  process.exit(1);
}
JS

# test_public_install_has_no_private_ops_frontend_release
[[ ! -e "$root/scripts/frontend_release.py" ]] || fail "scripts/frontend_release.py must not exist"
[[ ! -e "$root/scripts/frontend-release-test.py" ]] || fail "scripts/frontend-release-test.py must not exist"
[[ ! -e "$root/scripts/frontend_publication.py" ]] || fail "scripts/frontend_publication.py must not exist"
[[ ! -e "$root/docs/examples/private-ops" ]] || fail "docs/examples/private-ops must not exist"
for relative in \
  docs/INSTALL.md \
  docs/DEPLOY-BUNNY.md \
  docs/WEBUI.md \
  docs/OVERHAUL.md \
  docs/UPGRADE.md \
  docs/BUILDKITE.md \
  AGENTS.md \
  README.md \
  scripts/ci-preflight.sh \
  scripts/publish-frontend-bunny.sh \
  scripts/frontend_publication.sh \
  crates/conductor/src/install.rs
do
  require_absent "$relative" "frontend_release.py"
  require_absent "$relative" "frontend-release-test.py"
  require_absent "$relative" "frontend_publication.py"
  require_absent "$relative" "examples/private-ops"
  require_absent "$relative" "optional extra lock"
  require_absent "$relative" "Reverting an ops release"
  require_absent "$relative" "in-flight runs"
done
require_absent crates/conductor/frontend/app.js "Private ops automation"
require_contains crates/conductor/frontend/app.js "private ops repository is not required"

# test_cost_formulas_are_measured_not_invented
if ! grep -Fiq "local measurements" "$root/docs/COST.md"; then
  fail "missing in docs/COST.md: local measurements"
fi
if ! grep -Fiq "does not invent" "$root/docs/COST.md"; then
  fail "missing in docs/COST.md: does not invent"
fi
require_contains docs/COST.md "256 MiB"
if ! grep -Fiq "list prices" "$root/docs/COST.md"; then
  fail "missing in docs/COST.md: list prices"
fi

# test_validate_contract_is_check_not_apply_across_operator_surfaces
for relative in \
  docs/INSTALL.md \
  docs/OVERHAUL.md \
  docs/CONDUCTOR.md \
  docs/DEPLOY-AWS.md \
  docs/DEPLOY-BUNNY.md \
  docs/WEBUI.md \
  docs/SETUP.md \
  AGENTS.md \
  README.md \
  crates/conductor/src/install.rs \
  crates/conductor/frontend/app.js
do
  require_absent "$relative" "Install plan accepted"
  require_absent "$relative" "topologies, disjoint private roots, scope-limited URLs"
  require_absent "$relative" "The conductor checks topology, distinct private roots"
done
require_contains docs/INSTALL.md "does **not** apply process configuration"
require_contains docs/INSTALL.md "without requiring those local"
require_contains docs/INSTALL.md '`0700`'
require_contains docs/OVERHAUL.md "does not apply process configuration"
require_contains docs/OVERHAUL.md "without requiring those local paths"
require_contains docs/CONDUCTOR.md "without applying conductor"
require_contains crates/conductor/frontend/app.js "Install plan validated (not applied)"
require_contains crates/conductor/src/install.rs "does not apply process configuration"

# test_readme_points_at_public_install
require_contains README.md "docs/INSTALL.md"
require_contains README.md "docs/DEPLOY-AWS.md"
require_absent README.md "Point-and-click install remains later work"

# test_public_install_has_no_python_installer
[[ ! -e "$root/scripts/install-local.py" ]] || fail "scripts/install-local.py must not exist"
[[ ! -e "$root/scripts/public-install-acceptance-test.py" ]] || fail "scripts/public-install-acceptance-test.py must not exist"
require_absent docs/INSTALL.md "install-local.py"
require_absent docs/INSTALL.md "python3 scripts/install-local"
require_contains docs/INSTALL.md "cargo run --locked -p jmodguard-conductor --bin jmodguard-conductor -- init-local --root /tmp/jmodguard-disposable"
if grep -Fq "cargo run --locked -p jmodguard-conductor -- init-local" "$root/docs/INSTALL.md"; then
  fail "docs/INSTALL.md still has cargo run without --bin for init-local"
fi
require_contains docs/INSTALL.md "scripts/public-install-acceptance-test.sh"
require_contains docs/OVERHAUL.md "scripts/public-install-acceptance-test.sh"
require_contains scripts/ci-preflight.sh "bash scripts/public-install-acceptance-test.sh"
require_contains scripts/publish-frontend-bunny.sh "frontend_publication.sh"
require_absent scripts/publish-frontend-bunny.sh "frontend_publication.py"
require_absent scripts/ci-preflight.sh "public-install-acceptance-test.py"

# Remaining CI/tooling must not require Python.
for py in \
  scripts/check-service-boundaries.py \
  .buildkite/publish-image.py \
  .buildkite/publish-image-test.py \
  .buildkite/release.py \
  .buildkite/release-test.py \
  .buildkite/release-please.py
do
  [[ ! -e "$root/$py" ]] || fail "$py must not exist"
done
require_absent scripts/ci-preflight.sh "python3"
require_absent .buildkite/pipeline.yml "python3"
require_absent docs/BUILDKITE.md "Python 3"
require_absent docs/RELEASES.md "release-please.py"
require_absent docs/RELEASES.md "Python CI"
require_contains scripts/ci-preflight.sh "bash scripts/check-service-boundaries.sh"
require_contains scripts/ci-preflight.sh "bash .buildkite/publish-image-test.sh"
require_contains .buildkite/pipeline.yml "bash .buildkite/publish-image.sh"
require_contains .buildkite/pipeline.yml "bash .buildkite/release-please.sh"
require_contains docs/RELEASES.md "release-please.sh"
require_contains docs/BUILDKITE.md "bash .buildkite/publish-image-test.sh"
require_contains AGENTS.md "bash scripts/check-service-boundaries.sh"

echo 'public-install-acceptance-test: ok'
