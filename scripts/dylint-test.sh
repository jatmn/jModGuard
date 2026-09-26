#!/usr/bin/env bash
# Check the environment passed to Dylint without installing its toolchain.
set -euo pipefail

root="$(git rev-parse --show-toplevel)"
fixture="$(mktemp -d "${TMPDIR:-/tmp}/jmodguard-dylint-test.XXXXXX")"
trap 'rm -rf "$fixture"' EXIT
mkdir -p "$fixture/project/scripts" "$fixture/bin" "$fixture/external"
git init -q "$fixture/project"
cp "$root/scripts/dylint.sh" "$fixture/project/scripts/dylint.sh"
printf 'tag = "v6.1.0"\n' > "$fixture/project/dylint.toml"

cat > "$fixture/bin/cargo" <<'SH'
#!/usr/bin/env bash
set -euo pipefail
if [ "$1 $2" = 'install --list' ]; then
  printf 'cargo-dylint v6.1.0:\n    cargo-dylint\n\ndylint-link v6.1.0:\n    dylint-link\n' || :
elif [ "$1" = dylint ]; then
  printf '%s\n%s\n' "$TMPDIR" "$RUSTFLAGS" > "$DYLINT_TEST_CAPTURE"
else
  exit 1
fi
SH
chmod +x "$fixture/bin/cargo"

capture="$fixture/capture"
(cd "$fixture/project" && PATH="$fixture/bin:$PATH" DYLINT_TEST_CAPTURE="$capture" \
  TMPDIR="$fixture/project/.tmp/dylint" RUSTFLAGS='-C debuginfo=0' \
  bash scripts/dylint.sh)
actual_tmp="$(sed -n '1p' "$capture")"
case "$actual_tmp/" in
  "$fixture/project/"*) echo 'dylint test: temporary project stayed inside workspace' >&2; exit 1 ;;
esac
[ "$(sed -n '2p' "$capture")" = '-C debuginfo=0 -D warnings -A deprecated' ]

(cd "$fixture/project" && PATH="$fixture/bin:$PATH" DYLINT_TEST_CAPTURE="$capture" \
  TMPDIR="$fixture/external" RUSTFLAGS='-C debuginfo=0' \
  bash scripts/dylint.sh)
[ "$(sed -n '1p' "$capture")" = "$fixture/external" ]

# Caller flags may contain the policy flags before a later override. The
# Dylint flags must be last so both lint levels retain their intended effect.
caller_flags='-D warnings -A warnings -A deprecated -D deprecated'
(cd "$fixture/project" && PATH="$fixture/bin:$PATH" DYLINT_TEST_CAPTURE="$capture" \
  TMPDIR="$fixture/external" RUSTFLAGS="$caller_flags" \
  bash scripts/dylint.sh)
[ "$(sed -n '2p' "$capture")" = "$caller_flags -D warnings -A deprecated" ]
echo 'dylint test: isolated temporary directory and caller flags verified'
