#!/usr/bin/env bash
# Check the environment passed to Dylint without installing its toolchain.
# shellcheck disable=SC2030,SC2031
set -euo pipefail

root="$(git rev-parse --show-toplevel)"
fixture="$(mktemp -d "${TMPDIR:-/tmp}/jmodguard-dylint-test.XXXXXX")"
trap 'rm -rf "$fixture"' EXIT
mkdir -p "$fixture/project/scripts" "$fixture/bin" "$fixture/external"
git init -q "$fixture/project"
cp "$root/scripts/dylint.sh" "$fixture/project/scripts/dylint.sh"
cat > "$fixture/project/dylint.toml" <<'EOF'
[[workspace.metadata.dylint.libraries]]
tag = "v6.1.0"
EOF

fail() {
  echo "dylint test: $*" >&2
  exit 1
}

# Every stub logs the git config entries it inherits, so a test can tell
# which commands saw an entry.
cat > "$fixture/record-git-config.sh" <<'SH'
if [ -n "${DYLINT_TEST_GIT_CONFIG_LOG:-}" ]; then
  {
    printf '%s %s:' "${0##*/}" "$*"
    env | awk '/^GIT_CONFIG_/' | sort | tr '\n' ' '
    echo
  } >>"$DYLINT_TEST_GIT_CONFIG_LOG"
fi
SH

cat > "$fixture/bin/cargo" <<'SH'
#!/usr/bin/env bash
set -euo pipefail
# shellcheck source=/dev/null
[ -z "${DYLINT_TEST_RECORDER:-}" ] || source "$DYLINT_TEST_RECORDER"
field() {
  local name="$1"
  if [ -n "${!name+x}" ]; then
    printf '%s=%s\n' "$name" "${!name}"
  else
    printf '%s=unset\n' "$name"
  fi
}
if [ "${1:-}" = install ] && [ "${2:-}" = --list ]; then
  if [ "${3:-}" = --root ]; then
    ver="${DYLINT_TEST_ROOT_VERSION:-6.1.0}"
  else
    ver="${DYLINT_TEST_LISTED_VERSION:-6.1.0}"
  fi
  printf 'cargo-dylint v%s:\n    cargo-dylint\n\ndylint-link v%s:\n    dylint-link\n' "$ver" "$ver"
  if [ -n "${DYLINT_TEST_LARGE_CARGO_LIST:-}" ]; then
    cat "$DYLINT_TEST_LARGE_CARGO_LIST"
  fi
elif [ "${1:-}" = install ]; then
  {
    field RUSTFLAGS
    field CARGO_ENCODED_RUSTFLAGS
    printf 'argv=%s\n' "$*"
  } > "$DYLINT_TEST_INSTALL_CAPTURE"
elif [ "${1:-}" = dylint ]; then
  if [ "${2:-}" = --help ]; then
    if [ "${DYLINT_TEST_HELP_NO_METADATA:-1}" = 0 ]; then
      printf '%s\n' '--all'
    else
      printf '%s\n' '--no-metadata'
    fi
    exit 0
  fi
  {
    field TMPDIR
    field CARGO_HOME
    field RUSTFLAGS
    field CARGO_ENCODED_RUSTFLAGS
    field DYLINT_RUSTFLAGS
    field CARGO_TARGET_DIR
    field PATH
    field DYLINT_LIBRARY_PATH
    for name in GIT_CONFIG_COUNT GIT_CONFIG_KEY_0 GIT_CONFIG_VALUE_0 GIT_CONFIG_KEY_1 GIT_CONFIG_VALUE_1 GIT_CONFIG_KEY_2; do
      field "$name"
    done
    printf 'argv=%s\n' "$*"
  } > "$DYLINT_TEST_CAPTURE"
else
  echo "dylint test cargo: unexpected invocation: $*" >&2
  exit 1
fi
SH
chmod +x "$fixture/bin/cargo"

cat > "$fixture/bin/git" <<'SH'
#!/usr/bin/env bash
set -euo pipefail
# shellcheck source=/dev/null
[ -z "${DYLINT_TEST_RECORDER:-}" ] || source "$DYLINT_TEST_RECORDER"
if [ "${1:-}" = ls-remote ]; then
  printf '%s refs/tags/v6.1.0\n' '8aeea4b0c27722d46fcc8288330401418328e8fa'
  exit 0
fi
exec /usr/bin/git "$@"
SH
chmod +x "$fixture/bin/git"

cat > "$fixture/bin/curl" <<'SH'
#!/usr/bin/env bash
set -euo pipefail
# shellcheck source=/dev/null
[ -z "${DYLINT_TEST_RECORDER:-}" ] || source "$DYLINT_TEST_RECORDER"
if [ "${DYLINT_TEST_CURL_FAIL:-0}" = 1 ]; then
  echo "dylint test curl: network down" >&2
  exit 1
fi
printf '%s\n' '[toolchain]' 'channel = "nightly-2026-08-20"' 'components = ["llvm-tools-preview", "rustc-dev"]'
SH
chmod +x "$fixture/bin/curl"

cat > "$fixture/bin/rustc" <<'SH'
#!/usr/bin/env bash
set -euo pipefail
# shellcheck source=/dev/null
[ -z "${DYLINT_TEST_RECORDER:-}" ] || source "$DYLINT_TEST_RECORDER"
if [ "${DYLINT_TEST_RUSTC_FAIL:-0}" = 1 ]; then
  exit 1
fi
if [ -n "${DYLINT_TEST_TOOLCHAIN_STATE:-}" ] && [ ! -f "$DYLINT_TEST_TOOLCHAIN_STATE" ]; then
  echo 'dylint test rustc: toolchain is not installed (auto-install disabled)' >&2
  exit 1
fi
if [ "${1:-}" = '+nightly-2026-08-20' ] && [ "${2:-}" = -vV ]; then
  printf '%s\n' 'commit-hash: abcdefabcdefabcdefabcdefabcdefabcdefabcd' 'host: x86_64-unknown-linux-gnu'
  exit 0
fi
echo "dylint test rustc: unexpected invocation: $*" >&2
exit 1
SH
chmod +x "$fixture/bin/rustc"

cat > "$fixture/bin/rustup" <<'SH'
#!/usr/bin/env bash
set -euo pipefail
# shellcheck source=/dev/null
[ -z "${DYLINT_TEST_RECORDER:-}" ] || source "$DYLINT_TEST_RECORDER"
case "$*" in
  'component list --toolchain nightly-2026-08-20')
    state="${DYLINT_TEST_TOOLCHAIN_STATE:-}"
    [ -n "$state" ] && [ -f "$state" ] || exit 1
    if [ "${DYLINT_TEST_PARTIAL_COMPONENTS:-0}" = 1 ]; then
      printf '%s\n' 'llvm-tools-x86_64-unknown-linux-gnu (installed)'
      exit 0
    fi
    printf '%s\n' 'llvm-tools-x86_64-unknown-linux-gnu (installed)' 'rustc-dev-x86_64-unknown-linux-gnu (installed)'
    if [ -n "${DYLINT_TEST_LARGE_COMPONENT_LIST:-}" ]; then
      cat "$DYLINT_TEST_LARGE_COMPONENT_LIST"
    fi
    ;;
  'toolchain install nightly-2026-08-20 --component llvm-tools-preview,rustc-dev')
    if [ "${DYLINT_TEST_RUSTUP_FAIL:-0}" = 1 ]; then
      echo 'dylint test rustup: install failed' >&2
      exit 1
    fi
    echo installed >>"${DYLINT_TEST_TOOLCHAIN_STATE:?}"
    ;;
  *) echo "dylint test rustup: unexpected invocation: $*" >&2; exit 1 ;;
esac
SH
chmod +x "$fixture/bin/rustup"
touch "$fixture/toolchain-ready"

capture="$fixture/capture"
install_capture="$fixture/install-capture"
stdout_file="$fixture/stdout"
git_config_log="$fixture/git-config-log"

value() {
  local file="$1" key="$2" line
  while IFS= read -r line; do
    case "$line" in
      "$key="*) printf '%s\n' "${line#"$key="}"; return 0 ;;
    esac
  done < "$file"
  fail "missing $key"
}

expect() {
  local file="$1" key="$2" expected="$3" actual
  actual="$(value "$file" "$key")"
  if [ "$actual" != "$expected" ]; then
    fail "$key expected [$expected] got [$actual]"
  fi
}

run_dylint() {
  local tmpdir="$1"
  shift
  (
    cd "$fixture/project"
    JMODGUARD_CI_PREFIX="$fixture/opt" \
    PATH="$fixture/bin:$PATH" \
      DYLINT_TEST_CAPTURE="$capture" \
      DYLINT_TEST_INSTALL_CAPTURE="$install_capture" \
      DYLINT_TEST_TOOLCHAIN_STATE="${DYLINT_TEST_TOOLCHAIN_STATE:-$fixture/toolchain-ready}" \
      DYLINT_TEST_RECORDER="$fixture/record-git-config.sh" \
      DYLINT_TEST_GIT_CONFIG_LOG="$git_config_log" \
      TMPDIR="$tmpdir" \
      env -u CARGO_TARGET_DIR -u JMODGUARD_DYLINT_PACKAGES -u DYLINT_CARGO_HOME \
        -u GIT_CONFIG_COUNT -u GIT_CONFIG_KEY_0 -u GIT_CONFIG_VALUE_0 \
        "$@" bash scripts/dylint.sh >"$stdout_file" 2>&1
  )
}

inside="$fixture/project/.tmp/dylint"
run_dylint "$inside" \
  RUSTFLAGS='-C debuginfo=0' CARGO_ENCODED_RUSTFLAGS='-D warnings'
actual_tmp="$(value "$capture" TMPDIR)"
case "$actual_tmp/" in
  "$fixture/project/"*) fail "temporary project stayed inside workspace: $actual_tmp" ;;
esac
expect "$capture" TMPDIR /tmp
expect "$capture" CARGO_HOME "$inside/cargo-home"
expect "$capture" RUSTFLAGS unset
expect "$capture" CARGO_ENCODED_RUSTFLAGS unset
expect "$capture" DYLINT_RUSTFLAGS '-C debuginfo=0 -D warnings -A deprecated'
expect "$capture" CARGO_TARGET_DIR target-dylint

run_dylint "$fixture/external" \
  RUSTFLAGS='-C debuginfo=0' CARGO_ENCODED_RUSTFLAGS='-D warnings'
expect "$capture" TMPDIR "$fixture/external"
expect "$capture" CARGO_HOME "$fixture/external/cargo-home"

# Hosted CI supplies a persistent cache; an explicit override still takes effect.
run_dylint "$fixture/external" DYLINT_CARGO_HOME="$fixture/hosted-cache"
expect "$capture" CARGO_HOME "$fixture/hosted-cache"

# Caller flags may contain the policy flags before a later override. The
# Dylint flags must be last so both lint levels retain their intended effect.
caller_flags='-D warnings -A warnings -A deprecated -D deprecated'
run_dylint "$fixture/external" RUSTFLAGS="$caller_flags"
expect "$capture" RUSTFLAGS unset
expect "$capture" DYLINT_RUSTFLAGS "$caller_flags -D warnings -A deprecated"
expect "$capture" argv 'dylint --all --fail-on-no-libraries -- --locked --all-targets --workspace'

# A mismatched installed version must run `cargo install` without cargo's
# rustc-flag variables. The workspace policy still travels in DYLINT_RUSTFLAGS.
rm -f "$install_capture"
run_dylint "$fixture/external" \
  DYLINT_TEST_LISTED_VERSION='0.0.0' \
  RUSTFLAGS='-Z unstable-options' CARGO_ENCODED_RUSTFLAGS='-D warnings'
expect "$install_capture" RUSTFLAGS unset
expect "$install_capture" CARGO_ENCODED_RUSTFLAGS unset
expect "$install_capture" argv 'install cargo-dylint dylint-link --locked --version 6.1.0 --force'
expect "$capture" RUSTFLAGS unset
expect "$capture" DYLINT_RUSTFLAGS '-Z unstable-options -D warnings -A deprecated'
# A scoped CI lint must check the selected targets, not the whole workspace.
run_dylint "$fixture/external" JMODGUARD_DYLINT_PACKAGES='jmodguard-api jmodguard-integration-tests'
expect "$capture" argv 'dylint --all --fail-on-no-libraries -- --locked --all-targets -p jmodguard-api -p jmodguard-integration-tests'
if run_dylint "$fixture/external" JMODGUARD_DYLINT_PACKAGES='--workspace' >/dev/null 2>&1; then
  fail 'invalid package scope must fail, not become Cargo options'
fi
run_dylint "$fixture/external" JMODGUARD_DYLINT_PACKAGES=''
expect "$capture" argv 'dylint --all --fail-on-no-libraries -- --locked --all-targets --workspace'

# Listings larger than a pipe buffer, with the wanted entries first. If a
# listing is piped into a reader that stops at the first match, the stub is
# killed by SIGPIPE before it finishes and pipefail reports exit 141.
large_listing="$fixture/large-listing"
awk 'BEGIN { for (i = 0; i < 20000; i++) printf "    filler-entry-%05d (available)\n", i }' >"$large_listing"
[ "$(wc -c <"$large_listing")" -gt 262144 ] || fail 'large listing fixture is smaller than intended'
large_state="$fixture/large-listing-toolchain"
touch "$large_state"
run_dylint "$fixture/external" \
  DYLINT_TEST_TOOLCHAIN_STATE="$large_state" DYLINT_TEST_LARGE_COMPONENT_LIST="$large_listing" \
  || fail "large rustup component listing failed dylint (exit $?)"
grep -Fq 'toolchain-nightly-2026-08-20: already-present' "$stdout_file" \
  || fail 'large rustup component listing hid installed components'
[ ! -s "$large_state" ] || fail 'large rustup component listing reinstalled a complete nightly'
rm -f "$install_capture"
run_dylint "$fixture/external" DYLINT_TEST_LARGE_CARGO_LIST="$large_listing" \
  || fail "large cargo install listing failed dylint (exit $?)"
[ ! -f "$install_capture" ] || fail 'large cargo install listing reinstalled matching tools'

# A cold dylint 6.1.0 driver build clones rust-lang/rust-clippy and checks out
# `master`, which upstream renamed to `main`. Only `cargo dylint` gets the
# refspec that stores `main` as tag `master`, after the caller's entries.
clippy_refspec='+refs/heads/main*:refs/tags/master*'
run_dylint "$fixture/external"
expect "$capture" GIT_CONFIG_COUNT 1
expect "$capture" GIT_CONFIG_KEY_0 remote.origin.fetch
expect "$capture" GIT_CONFIG_VALUE_0 "$clippy_refspec"
expect "$capture" GIT_CONFIG_KEY_1 unset
rm -f "$git_config_log"
caller_helper='!f() { echo username=x; }; f'
run_dylint "$fixture/external" \
  GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=credential.helper GIT_CONFIG_VALUE_0="$caller_helper"
expect "$capture" GIT_CONFIG_COUNT 2
expect "$capture" GIT_CONFIG_KEY_0 credential.helper
expect "$capture" GIT_CONFIG_VALUE_0 "$caller_helper"
expect "$capture" GIT_CONFIG_KEY_1 remote.origin.fetch
expect "$capture" GIT_CONFIG_VALUE_1 "$clippy_refspec"
expect "$capture" GIT_CONFIG_KEY_2 unset
grep -q '^cargo dylint --all' "$git_config_log" || fail 'cargo dylint run was not logged'
other_commands="$(grep -v '^cargo dylint --all' "$git_config_log")"
[ -n "$other_commands" ] || fail 'no other command was logged'
if grep -Fq -- 'refs/tags/master' <<<"$other_commands"; then
  fail "clippy refspec leaked to another command: $(grep -F -- 'refs/tags/master' <<<"$other_commands" | head -n 1)"
fi
if grep -Fvq -- 'GIT_CONFIG_COUNT=1 ' <<<"$other_commands"; then
  fail "caller git config changed for another command: $(grep -Fv -- 'GIT_CONFIG_COUNT=1 ' <<<"$other_commands" | head -n 1)"
fi
if run_dylint "$fixture/external" GIT_CONFIG_COUNT=one >/dev/null 2>&1; then
  fail 'non-numeric GIT_CONFIG_COUNT must abort'
fi

# Real git with that environment: `master` names upstream `main`, and a
# repository without `main` still clones.
git_env=(
  "GIT_CONFIG_COUNT=$(value "$capture" GIT_CONFIG_COUNT)"
  "GIT_CONFIG_KEY_0=$(value "$capture" GIT_CONFIG_KEY_0)"
  "GIT_CONFIG_VALUE_0=$(value "$capture" GIT_CONFIG_VALUE_0)"
  "GIT_CONFIG_KEY_1=$(value "$capture" GIT_CONFIG_KEY_1)"
  "GIT_CONFIG_VALUE_1=$(value "$capture" GIT_CONFIG_VALUE_1)"
)
for branch in main trunk; do
  /usr/bin/git init -q -b "$branch" "$fixture/upstream-$branch"
  /usr/bin/git -C "$fixture/upstream-$branch" -c user.name=t -c user.email=t@example.invalid -c commit.gpgsign=false \
    commit -q --allow-empty -m "$branch"
  env "${git_env[@]}" /usr/bin/git clone -q "file://$fixture/upstream-$branch" "$fixture/clone-$branch" \
    || fail "clone of a repository with only $branch failed"
done
[ "$(/usr/bin/git -C "$fixture/clone-main" rev-parse master)" = \
  "$(/usr/bin/git -C "$fixture/upstream-main" rev-parse main)" ] \
  || fail 'master does not resolve to upstream main'
/usr/bin/git -C "$fixture/clone-main" rev-parse -q --verify refs/remotes/origin/main >/dev/null \
  || fail 'default fetch refspec was replaced'

prefix="$fixture/opt"
pinned_root="$prefix/dylint-6.1.0"
mkdir -p "$pinned_root/bin"
touch "$pinned_root/.crates.toml"
printf '# stub\n' >"$pinned_root/bin/cargo-dylint"
printf '# stub\n' >"$pinned_root/bin/dylint-link"
chmod +x "$pinned_root/bin/cargo-dylint" "$pinned_root/bin/dylint-link"
rm -f "$install_capture"
run_dylint "$fixture/external"
if [ -f "$install_capture" ]; then
  fail 'pinned tools must not run cargo install'
fi
grep -Fq 'dylint: using pinned tools at' "$stdout_file" || fail 'missing pinned tools message'
grep -Fq 'dylint-libs absent' "$stdout_file" || fail 'missing dylint-libs absent message'
run_dylint "$fixture/external" DYLINT_TEST_LARGE_CARGO_LIST="$large_listing" \
  || fail "large pinned cargo install listing failed dylint (exit $?)"
grep -Fq 'dylint: using pinned tools at' "$stdout_file" \
  || fail 'large pinned cargo install listing hid the pinned tools'
[ ! -f "$install_capture" ] || fail 'large pinned cargo install listing ran cargo install'
path_prefix="$(value "$capture" PATH)"
case "$path_prefix" in
  "$pinned_root/bin:"*) ;;
  *) fail "PATH must start with pinned bin: $path_prefix" ;;
esac
expect "$capture" argv 'dylint --all --fail-on-no-libraries -- --locked --all-targets --workspace'

rm -f "$install_capture"
run_dylint "$fixture/external" \
  DYLINT_TEST_ROOT_VERSION='0.0.0' \
  DYLINT_TEST_LISTED_VERSION='0.0.0'
grep -Fq 'unavailable or not 6.1.0' "$stdout_file" || fail 'missing unavailable pinned tools message'
expect "$install_capture" argv 'install cargo-dylint dylint-link --locked --version 6.1.0 --force'

# Load shared dylint helpers without running the main dylint driver.
sed '/^mapfile -t library_tags/,$d' "$fixture/project/scripts/dylint.sh" > "$fixture/setup-functions.sh"
(
  cd "$fixture/project"
  export PATH="$fixture/bin:$PATH"
  export DYLINT_TEST_TOOLCHAIN_STATE="$fixture/nightly-installed"
  # shellcheck source=/dev/null
  source "$fixture/setup-functions.sh"
  ensure_dylint_toolchain
  jmodguard_dylint_lib_key dylint.toml "$fixture/provisioned-key"
  grep -Fq 'toolchain=nightly-2026-08-20 abcdefabcdefabcdefabcdefabcdefabcdefabcd' "$fixture/provisioned-key" \
    || fail 'provisioning did not produce the installed compiler identity'
  ensure_dylint_toolchain
  [ "$(wc -l < "$DYLINT_TEST_TOOLCHAIN_STATE")" -eq 1 ] \
    || fail 'provisioning reinstalled an already complete nightly'
)

partial_state="$fixture/partial-toolchain"
touch "$partial_state"
run_dylint "$fixture/external" \
  DYLINT_TEST_TOOLCHAIN_STATE="$partial_state" DYLINT_TEST_PARTIAL_COMPONENTS=1
grep -Fq 'toolchain-nightly-2026-08-20: installed' "$stdout_file" \
  || fail 'missing toolchain install for partial components'
[ "$(wc -l < "$partial_state")" -eq 1 ] || fail 'partial toolchain install did not record state'

if run_dylint "$fixture/external" \
  DYLINT_TEST_TOOLCHAIN_STATE="$fixture/missing-toolchain" DYLINT_TEST_RUSTUP_FAIL=1 >/dev/null 2>&1; then
  fail 'rustup install failure must abort dylint'
fi

key_body_file="$fixture/key-body.txt"
lib_key_dir="$prefix/dylint-libs"
sha="$(sha256sum "$fixture/project/dylint.toml" | awk '{print $1}')"
printf 'dylint_toml_sha256=%s\nlints_rev=8aeea4b0c27722d46fcc8288330401418328e8fa\ntoolchain=nightly-2026-08-20 abcdefabcdefabcdefabcdefabcdefabcdefabcd\n' "$sha" >"$key_body_file"
KEY=$(sha256sum "$key_body_file" | awk '{print substr($1, 1, 32)}')
lib_dir="$lib_key_dir/$KEY"
rm -rf "$lib_key_dir"
mkdir -p "$lib_dir"
cp "$key_body_file" "$lib_dir/key.txt"
tc='nightly-2026-08-20-x86_64-unknown-linux-gnu'
for name in abs_home_path await_holding_span_guard basic_dead_store crate_wide_allow incorrect_matches_operation wrong_serialize_struct_arg; do
  printf 'x' >"$lib_dir/lib${name}@${tc}.so"
done

run_dylint "$fixture/external"
expect "$capture" argv 'dylint --all --no-metadata --fail-on-no-libraries -- --locked --all-targets --workspace'
expect "$capture" DYLINT_LIBRARY_PATH "$lib_dir"
expect "$capture" GIT_CONFIG_VALUE_0 "$clippy_refspec"
grep -Fq 'using prebuilt libs at' "$stdout_file" || fail 'missing prebuilt libs message'

printf 'x' >"$lib_dir/libextra@${tc}.so"
run_dylint "$fixture/external"
grep -Fq 'does not contain exactly the 6 libraries' "$stdout_file" || fail 'missing extra library message'
expect "$capture" argv 'dylint --all --fail-on-no-libraries -- --locked --all-targets --workspace'
rm -f "$lib_dir/libextra@${tc}.so"

printf 'wrong\n' >"$lib_dir/key.txt"
run_dylint "$fixture/external"
grep -Fq 'missing or key.txt mismatch' "$stdout_file" || fail 'missing key mismatch message'
expect "$capture" argv 'dylint --all --fail-on-no-libraries -- --locked --all-targets --workspace'
cp "$key_body_file" "$lib_dir/key.txt"

rm -f "$lib_dir/libwrong_serialize_struct_arg@${tc}.so"
run_dylint "$fixture/external"
grep -Fq 'does not contain exactly the 6 libraries' "$stdout_file" || fail 'missing six libraries message'
expect "$capture" argv 'dylint --all --fail-on-no-libraries -- --locked --all-targets --workspace'
printf 'x' >"$lib_dir/libwrong_serialize_struct_arg@${tc}.so"

printf '' >"$lib_dir/libbasic_dead_store@${tc}.so"
run_dylint "$fixture/external"
grep -Fq 'does not contain exactly the 6 libraries' "$stdout_file" || fail 'missing six libraries message for empty so'
printf 'x' >"$lib_dir/libbasic_dead_store@${tc}.so"

run_dylint "$fixture/external" DYLINT_TEST_HELP_NO_METADATA=0
grep -Fq 'cargo dylint --help has no --no-metadata' "$stdout_file" || fail 'missing no-metadata help fallback'
expect "$capture" argv 'dylint --all --fail-on-no-libraries -- --locked --all-targets --workspace'

run_dylint "$fixture/external" DYLINT_TEST_RUSTC_FAIL=1
grep -Fq 'key lookup failed' "$stdout_file" || fail 'missing key lookup failed message'
expect "$capture" argv 'dylint --all --fail-on-no-libraries -- --locked --all-targets --workspace'

if run_dylint "$fixture/external" DYLINT_TEST_CURL_FAIL=1 >/dev/null 2>&1; then
  fail 'dylint toolchain lookup failure must abort'
fi
grep -Fq 'dylint toolchain lookup failed' "$stdout_file" \
  || fail 'missing dylint toolchain lookup failed message for curl'

# scripts/ci-dylint-libs.sh publishes the pinned layout dylint.sh consumes on
# hosted runners: the tools from the dylint install root and the six
# metadata-built libraries from the dylint target directory.
cp "$root/scripts/ci-dylint-libs.sh" "$fixture/project/scripts/ci-dylint-libs.sh"
publish_prefix="$fixture/publish-prefix"
publish_cargo="$fixture/publish-cargo"
publish_target="$fixture/publish-target"
provision_stdout="$fixture/provision-stdout"
publish_lib_dir="$publish_prefix/dylint-libs/$KEY"
mkdir -p "$publish_cargo/bin" "$publish_target/dylint/libraries/$tc/release"
printf '#!/bin/sh\nexit 0\n' >"$publish_cargo/bin/cargo-dylint"
printf '#!/bin/sh\nexit 0\n' >"$publish_cargo/bin/dylint-link"
chmod +x "$publish_cargo/bin/cargo-dylint" "$publish_cargo/bin/dylint-link"
printf 'cargo-dylint v6.1.0:\n    cargo-dylint\n\ndylint-link v6.1.0:\n    dylint-link\n' \
  >"$publish_cargo/.crates.toml"
for name in abs_home_path await_holding_span_guard basic_dead_store crate_wide_allow incorrect_matches_operation wrong_serialize_struct_arg; do
  printf 'x' >"$publish_target/dylint/libraries/$tc/release/lib${name}@${tc}.so"
done

run_provision() {
  local prefix="$1" cargo_home="$2" target_dir="$3"
  shift 3
  (
    cd "$fixture/project"
    JMODGUARD_CI_PREFIX="$prefix" \
    DYLINT_CARGO_HOME="$cargo_home" \
    CARGO_TARGET_DIR="$target_dir" \
    PATH="$fixture/bin:$PATH" \
    TMPDIR="$fixture/external" \
    env -u JMODGUARD_DYLINT_PACKAGES -u GIT_CONFIG_COUNT -u GIT_CONFIG_KEY_0 -u GIT_CONFIG_VALUE_0 \
      "$@" bash scripts/ci-dylint-libs.sh
  ) >"$provision_stdout" 2>&1
}

run_provision "$publish_prefix" "$publish_cargo" "$publish_target"
grep -Fq "ci-dylint-libs: published pinned dylint 6.1.0 tools to $publish_prefix/dylint-6.1.0" "$provision_stdout" \
  || fail 'provisioning must publish the pinned tools'
grep -Fq "ci-dylint-libs: published prebuilt lint libraries to $publish_lib_dir" "$provision_stdout" \
  || fail 'provisioning must publish the prebuilt libs'
[ -x "$publish_prefix/dylint-6.1.0/bin/cargo-dylint" ] || fail 'published pinned tool binary is missing'
[ -f "$publish_prefix/dylint-6.1.0/.crates.toml" ] || fail 'published pinned crates manifest is missing'
cmp -s "$publish_prefix/dylint-6.1.0/.crates.toml" "$publish_cargo/.crates.toml" \
  || fail 'published pinned tools must come from the dylint install root'
cmp -s "$publish_lib_dir/key.txt" "$key_body_file" || fail 'published libs must carry the computed key'
[ "$(find "$publish_lib_dir" -maxdepth 1 -type f -name '*.so' | wc -l)" -eq 6 ] \
  || fail 'published libs must contain exactly the six libraries'
cmp -s "$publish_lib_dir/libbasic_dead_store@${tc}.so" "$publish_target/dylint/libraries/$tc/release/libbasic_dead_store@${tc}.so" \
  || fail 'published libs must copy the metadata build output'

# A stale or corrupt entry under the same key is replaced, not trusted.
printf 'wrong\n' >"$publish_lib_dir/key.txt"
run_provision "$publish_prefix" "$publish_cargo" "$publish_target"
cmp -s "$publish_lib_dir/key.txt" "$key_body_file" || fail 'provisioning must replace a corrupt lib entry'
[ -z "$(find "$publish_prefix/dylint-libs" -maxdepth 1 -name '.publish.*' -print -quit)" ] \
  || fail 'provisioning must not leave staging directories'

# Publishing is idempotent: a second run reports and keeps the layout.
run_provision "$publish_prefix" "$publish_cargo" "$publish_target"
grep -Fq "ci-dylint-libs: pinned tools already published at $publish_prefix/dylint-6.1.0" "$provision_stdout" \
  || fail 'second provisioning run must report the pinned tools'
grep -Fq "ci-dylint-libs: prebuilt libs already published at $publish_lib_dir" "$provision_stdout" \
  || fail 'second provisioning run must report the prebuilt libs'

# dylint.sh consumes the published layout: pinned tools plus prebuilt libs
# with --no-metadata, with no per-run install and no metadata build.
rm -f "$capture" "$install_capture"
(
  cd "$fixture/project"
  JMODGUARD_CI_PREFIX="$publish_prefix" \
  PATH="$fixture/bin:$PATH" \
  DYLINT_TEST_CAPTURE="$capture" \
  DYLINT_TEST_INSTALL_CAPTURE="$install_capture" \
  DYLINT_TEST_TOOLCHAIN_STATE="$fixture/toolchain-ready" \
  DYLINT_TEST_RECORDER="$fixture/record-git-config.sh" \
  DYLINT_TEST_GIT_CONFIG_LOG="$git_config_log" \
  TMPDIR="$fixture/external" \
  env -u CARGO_TARGET_DIR -u JMODGUARD_DYLINT_PACKAGES -u DYLINT_CARGO_HOME \
    -u GIT_CONFIG_COUNT -u GIT_CONFIG_KEY_0 -u GIT_CONFIG_VALUE_0 \
    bash scripts/dylint.sh >"$stdout_file" 2>&1
)
grep -Fq "dylint: using pinned tools at $publish_prefix/dylint-6.1.0" "$stdout_file" \
  || fail 'dylint must use the published pinned tools'
grep -Fq "dylint: using prebuilt libs at $publish_lib_dir" "$stdout_file" \
  || fail 'dylint must use the published prebuilt libs'
if [ -f "$install_capture" ]; then
  fail 'dylint must not install tools when the published pinned layout is valid'
fi
expect "$capture" argv 'dylint --all --no-metadata --fail-on-no-libraries -- --locked --all-targets --workspace'
expect "$capture" DYLINT_LIBRARY_PATH "$publish_lib_dir"

# A dylint tag bump moves the pinned tool root for the publisher and dylint.sh
# together: both derive dylint-<tag> from dylint.toml.
cp "$fixture/project/dylint.toml" "$fixture/dylint.toml.orig"
sed -i 's/^tag = "v6\.1\.0"$/tag = "v6.2.0"/' "$fixture/project/dylint.toml"
bump_prefix="$fixture/bump-prefix"
run_provision "$bump_prefix" "$publish_cargo" "$publish_target" DYLINT_TEST_ROOT_VERSION=6.2.0
grep -Fq "ci-dylint-libs: published pinned dylint 6.2.0 tools to $bump_prefix/dylint-6.2.0" "$provision_stdout" \
  || fail 'provisioning must publish the pinned tools for the bumped tag'
run_dylint "$fixture/external" JMODGUARD_CI_PREFIX="$bump_prefix" \
  DYLINT_TEST_ROOT_VERSION=6.2.0 DYLINT_TEST_LISTED_VERSION=6.2.0
grep -Fq "dylint: using pinned tools at $bump_prefix/dylint-6.2.0" "$stdout_file" \
  || fail 'dylint must consume the pinned tools published for the bumped tag'
cp "$fixture/dylint.toml.orig" "$fixture/project/dylint.toml"

# A run that cannot publish a complete layout fails without creating entries:
# actions/cache saves only after a successful job and never re-saves a
# primary-key hit, so an incomplete entry would persist for every later run.
skip_prefix="$fixture/skip-prefix"
if run_provision "$skip_prefix" "$fixture/external/cargo-home" "$fixture/external"; then
  fail 'provisioning without dylint tools must fail'
fi
grep -Fq 'no dylint tools under' "$provision_stdout" || fail 'missing tools must be reported'
[ ! -d "$skip_prefix/dylint-libs" ] || fail 'a failed run must not create dylint-libs'
[ ! -d "$skip_prefix/dylint-6.1.0" ] || fail 'a failed run must not create the pinned tool root'

if run_provision "$skip_prefix" "$publish_cargo" "$fixture/external"; then
  fail 'provisioning without metadata-built libraries must fail'
fi
grep -Fq 'has no lib' "$provision_stdout" || fail 'missing libraries must be reported'
[ ! -d "$skip_prefix/dylint-libs" ] || fail 'a failed run must not create dylint-libs'

if run_provision "$skip_prefix" "$publish_cargo" "$publish_target" DYLINT_TEST_CURL_FAIL=1; then
  fail 'provisioning with a failed library key lookup must fail'
fi
grep -Fq 'library key lookup failed' "$provision_stdout" || fail 'key lookup failure must be reported'
[ ! -d "$skip_prefix/dylint-libs" ] || fail 'a failed key lookup must not create dylint-libs'

while IFS= read -r line; do
  if [[ "$line" == *dylint-libs* ]]; then
    if [[ "$line" == *'rm '* ]] || [[ "$line" == *'mv '* ]] || [[ "$line" == *'cp '* ]] || [[ "$line" == *'>'* ]]; then
      fail "dylint.sh must not mutate dylint-libs: $line"
    fi
  fi
done <"$fixture/project/scripts/dylint.sh"

echo 'dylint test: isolated temporary directory and caller flags verified'
