#!/usr/bin/env bash
# Upload frontend/ to a Bunny Storage Zone and optionally purge a Pull Zone.
# Designed to run on GitHub Actions in a *private* ops repo (Bunny credentials
# never belong in the public jModGuard repository).
set -euo pipefail

zone="${BUNNY_STORAGE_ZONE:?set BUNNY_STORAGE_ZONE}"
password="${BUNNY_STORAGE_PASSWORD:?set BUNNY_STORAGE_PASSWORD}"
endpoint="${BUNNY_STORAGE_ENDPOINT:-storage.bunnycdn.com}"
endpoint="${endpoint#https://}"
endpoint="${endpoint%/}"

root="$(cd "$(dirname "$0")/.." && pwd)/frontend"
if [[ ! -f "$root/index.html" || ! -f "$root/app.js" || ! -f "$root/style.css" ]]; then
  echo "frontend/ is missing index.html, app.js, or style.css under $root" >&2
  exit 1
fi

backup_dir="$(mktemp -d)"
trap 'rm -rf "$backup_dir"' EXIT

storage_url() {
  printf 'https://%s/%s/%s' "$endpoint" "$zone" "$1"
}

download_existing() {
  local file="$1"
  local dest="$backup_dir/$file"
  local code
  code="$(curl -sS -o "$dest" -w '%{http_code}' \
    -H "AccessKey: ${password}" \
    "$(storage_url "$file")" || true)"
  if [[ "$code" == "200" ]]; then
    echo "backed up existing ${file}"
    return 0
  fi
  rm -f "$dest"
  echo "no existing ${file} to restore (HTTP ${code})"
}

restore_existing() {
  local file="$1"
  local dest="$backup_dir/$file"
  if [[ -f "$dest" ]]; then
    echo "restoring previous ${file}"
    curl -fsS -X PUT "$(storage_url "$file")" \
      -H "AccessKey: ${password}" \
      --upload-file "$dest"
  fi
}

rollback() {
  echo "upload failed; restoring previous frontend/ files" >&2
  restore_existing app.js || true
  restore_existing style.css || true
  restore_existing index.html || true
}

upload() {
  local file="$1"
  local mime="$2"
  echo "PUT ${file} -> ${endpoint}/${zone}/${file}"
  curl -fsS -X PUT "$(storage_url "$file")" \
    -H "AccessKey: ${password}" \
    -H "Content-Type: ${mime}" \
    --upload-file "${root}/${file}"
}

download_existing index.html
download_existing app.js
download_existing style.css

# Assets first, HTML last, so a failed later PUT cannot pair new HTML with
# old JS/CSS. On any upload failure, restore the previous Storage copies.
if ! upload app.js "application/javascript; charset=utf-8"; then
  rollback
  exit 1
fi
if ! upload style.css "text/css; charset=utf-8"; then
  rollback
  exit 1
fi
if ! upload index.html "text/html; charset=utf-8"; then
  rollback
  exit 1
fi

if [[ -n "${BUNNY_PULL_ZONE_ID:-}" ]]; then
  key="${BUNNY_API_KEY:?set BUNNY_API_KEY to purge the Pull Zone}"
  echo "Purging Pull Zone ${BUNNY_PULL_ZONE_ID}"
  purged=0
  for _ in 1 2 3 4 5 6; do
    if curl -fsS -X POST "https://api.bunny.net/pullzone/${BUNNY_PULL_ZONE_ID}/purgeCache" \
      -H "AccessKey: ${key}" \
      -H "Content-Type: application/json" \
      -d '{}'; then
      purged=1
      break
    fi
    sleep 2
  done
  if [[ "$purged" -ne 1 ]]; then
    echo "Storage upload succeeded but Pull Zone purge failed. Purge zone ${BUNNY_PULL_ZONE_ID} in the Bunny dashboard or CDN will keep stale assets." >&2
    exit 1
  fi
fi

echo "frontend/ published"
