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

upload() {
  local file="$1"
  local mime="$2"
  echo "PUT ${file} -> ${endpoint}/${zone}/${file}"
  curl -fsS -X PUT "https://${endpoint}/${zone}/${file}" \
    -H "AccessKey: ${password}" \
    -H "Content-Type: ${mime}" \
    --upload-file "${root}/${file}"
}

upload index.html "text/html; charset=utf-8"
upload app.js "application/javascript; charset=utf-8"
upload style.css "text/css; charset=utf-8"

if [[ -n "${BUNNY_PULL_ZONE_ID:-}" ]]; then
  key="${BUNNY_API_KEY:?set BUNNY_API_KEY to purge the Pull Zone}"
  echo "Purging Pull Zone ${BUNNY_PULL_ZONE_ID}"
  curl -fsS -X POST "https://api.bunny.net/pullzone/${BUNNY_PULL_ZONE_ID}/purgeCache" \
    -H "AccessKey: ${key}" \
    -H "Content-Type: application/json" \
    -d '{}'
fi

echo "frontend/ published"
