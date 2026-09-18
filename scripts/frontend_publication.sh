#!/usr/bin/env bash
# Bunny publication receipt: asset hashes, never a source-commit checkpoint.
set -euo pipefail

FILES=(index.html app.js style.css)

fail() {
  echo "frontend_publication: $*" >&2
  exit 1
}

valid_zone() {
  [[ "$1" =~ ^[1-9][0-9]*$ ]]
}

configured_zone() {
  local zone="${BUNNY_PULL_ZONE_ID:-}"
  valid_zone "$zone" || fail "BUNNY_PULL_ZONE_ID must be a positive numeric Pull Zone ID"
  printf '%s' "$zone"
}

file_sha256() {
  local path="$1"
  sha256sum -- "$path" | awk '{print $1}'
}

asset_hash() {
  local root="$1"
  local name="$2"
  local path="$root/$name"
  if [[ -L "$path" || ! -f "$path" ]]; then
    fail "$name must be a regular asset"
  fi
  file_sha256 "$path"
}

json_escape() {
  printf '%s' "$1" | sed 's/\\/\\\\/g; s/"/\\"/g'
}

files_json() {
  local app_js="$1"
  local index_html="$2"
  local style_css="$3"
  if [[ -z "$app_js" && -z "$index_html" && -z "$style_css" ]]; then
    printf '{}'
    return
  fi
  printf '{"app.js": "%s", "index.html": "%s", "style.css": "%s"}' \
    "$(json_escape "$app_js")" \
    "$(json_escape "$index_html")" \
    "$(json_escape "$style_css")"
}

receipt_json() {
  local complete="$1"
  local zone="$2"
  local requires_api="$3"
  local files="$4"
  printf '{"complete": %s, "files": %s, "pull_zone_id": "%s", "requires_api": %s, "version": 1}' \
    "$complete" "$files" "$(json_escape "$zone")" "$requires_api"
}

json_bool_field() {
  local json="$1"
  local key="$2"
  printf '%s' "$json" | grep -oE "\"$key\": (true|false)" | head -n1 | awk '{print $2}'
}

json_string_field() {
  local json="$1"
  local key="$2"
  printf '%s' "$json" | grep -oE "\"$key\": \"[^\"]*\"" | head -n1 | sed -E "s/\"$key\": \"([^\"]*)\"/\1/"
}

json_file_hash() {
  local json="$1"
  local name="$2"
  printf '%s' "$json" | grep -oE "\"$name\": \"[0-9a-f]{64}\"" | head -n1 | sed -E "s/\"$name\": \"([0-9a-f]{64})\"/\1/"
}

parse_receipt() {
  local raw="$1"
  local zone="$2"
  PARSE_OK=0
  PARSE_COMPLETE=""
  PARSE_REQUIRES_API=""
  PARSE_APP=""
  PARSE_HTML=""
  PARSE_CSS=""
  PARSE_HAS_FILES=0
  valid_zone "$zone" || return 0
  printf '%s' "$raw" | grep -qE '"version": 1([,}])' || return 0
  local complete pull_zone requires_api
  complete="$(json_bool_field "$raw" complete)"
  pull_zone="$(json_string_field "$raw" pull_zone_id)"
  [[ "$complete" == "true" || "$complete" == "false" ]] || return 0
  [[ "$pull_zone" == "$zone" ]] || return 0
  requires_api="$(json_bool_field "$raw" requires_api)"
  if [[ -z "$requires_api" ]]; then
    if [[ "$complete" == "true" ]]; then
      requires_api="false"
    else
      requires_api="true"
    fi
  fi
  [[ "$requires_api" == "true" || "$requires_api" == "false" ]] || return 0
  PARSE_APP="$(json_file_hash "$raw" app.js)"
  PARSE_HTML="$(json_file_hash "$raw" index.html)"
  PARSE_CSS="$(json_file_hash "$raw" style.css)"
  if [[ -z "$PARSE_APP$PARSE_HTML$PARSE_CSS" ]]; then
    if [[ "$complete" == "true" ]]; then
      return 0
    fi
    PARSE_OK=1
    PARSE_COMPLETE="$complete"
    PARSE_REQUIRES_API="$requires_api"
    PARSE_HAS_FILES=0
    return 0
  fi
  if [[ -z "$PARSE_APP" || -z "$PARSE_HTML" || -z "$PARSE_CSS" ]]; then
    return 0
  fi
  PARSE_OK=1
  PARSE_COMPLETE="$complete"
  PARSE_REQUIRES_API="$requires_api"
  PARSE_HAS_FILES=1
}

make_receipt() {
  local mode="$1"
  local root="$2"
  local previous="$3"
  local zone="$4"
  local desired_html desired_js desired_css
  desired_html="$(asset_hash "$root" index.html)"
  desired_js="$(asset_hash "$root" app.js)"
  desired_css="$(asset_hash "$root" style.css)"
  PARSE_OK=0
  PARSE_COMPLETE=""
  PARSE_REQUIRES_API=""
  PARSE_APP=""
  PARSE_HTML=""
  PARSE_CSS=""
  PARSE_HAS_FILES=0
  if [[ -f "$previous" ]]; then
    parse_receipt "$(cat "$previous")" "$zone"
  fi
  local complete="false"
  local requires_api="false"
  local files
  if [[ "$mode" == "complete" ]]; then
    complete="true"
    files="$(files_json "$desired_js" "$desired_html" "$desired_css")"
    receipt_json "$complete" "$zone" "$requires_api" "$files"
    printf '\n'
    return
  fi
  if [[ "$PARSE_OK" -eq 0 ]]; then
    requires_api="true"
  elif [[ "$PARSE_COMPLETE" == "false" && "$PARSE_REQUIRES_API" == "true" ]]; then
    requires_api="true"
  fi
  local name backup hash
  for name in index.html app.js; do
    backup="$(dirname "$previous")/$name"
    case "$name" in
      index.html) hash="$desired_html" ;;
      app.js) hash="$desired_js" ;;
    esac
    local old_hash=""
    if [[ "$PARSE_OK" -eq 1 && "$PARSE_HAS_FILES" -eq 1 ]]; then
      case "$name" in
        index.html) old_hash="$PARSE_HTML" ;;
        app.js) old_hash="$PARSE_APP" ;;
      esac
    fi
    if [[ "$PARSE_OK" -eq 0 || "$old_hash" != "$hash" || ! -f "$backup" ]]; then
      requires_api="true"
      continue
    fi
    if [[ "$(file_sha256 "$backup")" != "$hash" ]]; then
      requires_api="true"
    fi
  done
  if [[ "$PARSE_OK" -eq 1 && "$PARSE_HAS_FILES" -eq 1 ]]; then
    files="$(files_json "$PARSE_APP" "$PARSE_HTML" "$PARSE_CSS")"
  else
    files="{}"
  fi
  receipt_json "$complete" "$zone" "$requires_api" "$files"
  printf '\n'
}

zone="$(configured_zone)"
if [[ "${1:-}" == "zone" && $# -eq 1 ]]; then
  printf '%s\n' "$zone"
  exit 0
fi
if [[ $# -ne 3 ]]; then
  fail "expected zone, or pending|complete ROOT PREVIOUS"
fi
mode="$1"
root="$2"
previous="$3"
[[ "$mode" == "pending" || "$mode" == "complete" ]] || fail "expected pending or complete"
make_receipt "$mode" "$root" "$previous" "$zone"
