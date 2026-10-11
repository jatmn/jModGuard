#!/usr/bin/env bash
# Check the environment passed to Dylint without installing its toolchain.
set -euo pipefail

root="$(git rev-parse --show-toplevel)"
fixture="$(mktemp -d "${TMPDIR:-/tmp}/jmodguard-dylint-test.XXXXXX")"
trap 'rm -rf "$fixture"' EXIT
mkdir -p "$fixture/project/scripts" "$fixture/bin" "$fixture/external"
git init -q "$fixture/project"
cp "$root/scripts/dylint.sh" "$fixture/project/scripts/dylint.sh"
cp "$root/scripts/ci-agent-setup.sh" "$fixture/ci-agent-setup.sh"
cat > "$fixture/project/dylint.toml" <<'EOF'
[[workspace.metadata.dylint.libraries]]
tag = "v6.1.0"
EOF

fail() {
  echo "dylint test: $*" >&2
  exit 1
}

cat > "$fixture/bin/cargo" <<'SH'
#!/usr/bin/env bash
set -euo pipefail
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
    printf 'argv=%s\n' "$*"
  } > "$DYLINT_TEST_CAPTURE"
else
  echo "dylint test cargo: unexpected invocation: $*" >&2
  exit 1
fi
SH
chmod +x "$fixture/bin/cargo"

capture="$fixture/capture"
install_capture="$fixture/install-capture"
stdout_file="$fixture/stdout"

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
      TMPDIR="$tmpdir" \
      env -u CARGO_TARGET_DIR -u JMODGUARD_DYLINT_PACKAGES "$@" bash scripts/dylint.sh >"$stdout_file"
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

cat > "$fixture/bin/git" <<'SH'
#!/usr/bin/env bash
set -euo pipefail
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

# Provisioning must explicitly install a missing nightly before querying its
# compiler identity, including when rustup's automatic installation is disabled.
cat > "$fixture/bin/rustup" <<'SH'
#!/usr/bin/env bash
set -euo pipefail
case "$*" in
  'component list --toolchain nightly-2026-08-20')
    [ -f "$DYLINT_TEST_TOOLCHAIN_STATE" ] || exit 1
    printf '%s\n' 'llvm-tools-x86_64-unknown-linux-gnu (installed)' 'rustc-dev-x86_64-unknown-linux-gnu (installed)'
    ;;
  'toolchain install nightly-2026-08-20 --component llvm-tools-preview,rustc-dev')
    echo installed >> "$DYLINT_TEST_TOOLCHAIN_STATE"
    ;;
  *) echo "dylint test rustup: unexpected invocation: $*" >&2; exit 1 ;;
esac
SH
chmod +x "$fixture/bin/rustup"
# Load the production functions without running the unrelated tool/library builds.
sed '/^ensure_main_toolchain$/,$d' "$fixture/ci-agent-setup.sh" > "$fixture/setup-functions.sh"
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

run_dylint "$fixture/external" DYLINT_TEST_CURL_FAIL=1
grep -Fq 'key lookup failed' "$stdout_file" || fail 'missing key lookup failed message for curl'
expect "$capture" argv 'dylint --all --fail-on-no-libraries -- --locked --all-targets --workspace'

setup_key="$(sed -n '/^# BEGIN dylint-lib-key$/,/^# END dylint-lib-key$/p' "$fixture/ci-agent-setup.sh")"
dylint_key="$(sed -n '/^# BEGIN dylint-lib-key$/,/^# END dylint-lib-key$/p' "$fixture/project/scripts/dylint.sh")"
if [ "$setup_key" != "$dylint_key" ]; then
  fail 'dylint-lib-key regions differ between ci-agent-setup.sh and dylint.sh'
fi

while IFS= read -r line; do
  if [[ "$line" == *dylint-libs* ]]; then
    if [[ "$line" == *'rm '* ]] || [[ "$line" == *'mv '* ]] || [[ "$line" == *'cp '* ]] || [[ "$line" == *'>'* ]]; then
      fail "dylint.sh must not mutate dylint-libs: $line"
    fi
  fi
done <"$fixture/project/scripts/dylint.sh"

echo 'dylint test: isolated temporary directory and caller flags verified'
