#!/usr/bin/env bash
# Check the environment passed to Dylint without installing its toolchain.
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
if [ "${1:-} ${2:-}" = 'install --list' ]; then
  ver="${DYLINT_TEST_LISTED_VERSION:-6.1.0}"
  printf 'cargo-dylint v%s:\n    cargo-dylint\n\ndylint-link v%s:\n    dylint-link\n' "$ver" "$ver"
elif [ "${1:-}" = install ]; then
  {
    field RUSTFLAGS
    field CARGO_ENCODED_RUSTFLAGS
    printf 'argv=%s\n' "$*"
  } > "$DYLINT_TEST_INSTALL_CAPTURE"
elif [ "${1:-}" = dylint ]; then
  {
    field TMPDIR
    field CARGO_HOME
    field RUSTFLAGS
    field CARGO_ENCODED_RUSTFLAGS
    field DYLINT_RUSTFLAGS
    field CARGO_TARGET_DIR
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
    PATH="$fixture/bin:$PATH" \
      DYLINT_TEST_CAPTURE="$capture" \
      DYLINT_TEST_INSTALL_CAPTURE="$install_capture" \
      TMPDIR="$tmpdir" \
      env -u CARGO_TARGET_DIR -u JMODGUARD_DYLINT_PACKAGES "$@" bash scripts/dylint.sh
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
echo 'dylint test: isolated temporary directory and caller flags verified'
