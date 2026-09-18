#!/usr/bin/env bash
# Upload frontend/ to a Bunny Storage Zone and purge the configured production Pull Zone.
#
# Public installs can upload the same files from this repository or from
# frontend/ paths in a signed release manifest. Keep Storage passwords out of git.
set -euo pipefail

pull_zone="$(bash "$(dirname "$0")/frontend_publication.sh" zone)"
key="${BUNNY_API_KEY:?set BUNNY_API_KEY to purge the Pull Zone}"

zone="${BUNNY_STORAGE_ZONE:?set BUNNY_STORAGE_ZONE}"
password="${BUNNY_STORAGE_PASSWORD:?set BUNNY_STORAGE_PASSWORD}"
endpoint="${BUNNY_STORAGE_ENDPOINT:-storage.bunnycdn.com}"
endpoint="${endpoint#https://}"
endpoint="${endpoint%/}"

root="${JMODGUARD_FRONTEND_ROOT:-$(cd "$(dirname "$0")/.." && pwd)/frontend}"
if [[ ! -f "$root/index.html" || ! -f "$root/app.js" || ! -f "$root/style.css" ]]; then
  echo "frontend/ is missing index.html, app.js, or style.css under $root" >&2
  exit 1
fi

backup_dir="$(mktemp -d)"
trap 'rm -rf "$backup_dir"' EXIT

storage_url() {
  printf 'https://%s/%s/%s' "$endpoint" "$zone" "$1"
}

# Retry a Storage mutating call. Failures here are the rollback/upload surface.
retry_sleep="${JMODGUARD_PUBLISH_RETRY_SLEEP:-1}"

storage_put() {
  local file="$1"
  local src="$2"
  local mime="${3:-}"
  local args=(-fsS -X PUT "$(storage_url "$file")" -H "AccessKey: ${password}" --upload-file "$src")
  if [[ -n "$mime" ]]; then
    args+=(-H "Content-Type: ${mime}")
  fi
  local i
  for i in 1 2 3; do
    if curl "${args[@]}"; then
      return 0
    fi
    sleep "$retry_sleep"
  done
  return 1
}

storage_delete() {
  local file="$1"
  local i
  for i in 1 2 3; do
    if curl -fsS -X DELETE "$(storage_url "$file")" \
      -H "AccessKey: ${password}"; then
      return 0
    fi
    sleep "$retry_sleep"
  done
  return 1
}

# Classify existing objects before any PUT. Non-200/404 is not "missing":
# a failed backup must not later DELETE a live object.
download_existing() {
  local file="$1"
  local dest="$backup_dir/$file"
  local code
  if ! code="$(curl -sS -o "$dest" -w '%{http_code}' \
    -H "AccessKey: ${password}" \
    "$(storage_url "$file")")"; then
    echo "backup transfer failed for ${file}; refusing to publish" >&2
    exit 1
  fi
  if [[ "$code" == "200" ]]; then
    echo "backed up existing ${file}"
    return 0
  fi
  rm -f "$dest"
  if [[ "$code" == "404" ]]; then
    : >"$backup_dir/${file}.absent"
    echo "no existing ${file} to restore (HTTP 404)"
    return 0
  fi
  echo "cannot determine existing ${file} (HTTP ${code}); refusing to publish without a restore plan" >&2
  exit 1
}

rollback_once() {
  echo "restoring previous frontend/ files on Storage" >&2
  local file dest status=0
  for file in "$@"; do
    dest="$backup_dir/$file"
    if [[ -f "$dest" ]]; then
      echo "restoring previous ${file}" >&2
      if ! storage_put "$file" "$dest"; then
        echo "failed to restore ${file}" >&2
        status=1
      fi
    elif [[ -f "$backup_dir/${file}.absent" ]]; then
      echo "removing newly uploaded ${file} (confirmed missing before publish)" >&2
      if ! storage_delete "$file"; then
        echo "failed to delete ${file}; Storage may be inconsistent" >&2
        status=1
      fi
    else
      echo "no restore plan for ${file}; leaving Storage object unchanged" >&2
      status=1
    fi
  done
  return "$status"
}

# Restore is a set: retry until every uploaded file is restored or confirmed
# deleted. A single pass that continues after a failed PUT leaves mixed origin.
rollback() {
  local attempt
  for attempt in 1 2 3 4 5 6; do
    if rollback_once "$@"; then
      return 0
    fi
    sleep "$retry_sleep"
  done
  return 1
}

abort_upload() {
  echo "upload failed" >&2
  # Restore every frontend object we backed up, not only files curl reported
  # as uploaded. A PUT can be accepted by Storage and still look like a
  # client failure (lost response, retry timeout). Those objects would
  # otherwise stay on the new revision and mix with restored files.
  if ! rollback app.js style.css index.html; then
    echo "rollback did not fully restore Storage; the zone may mix revisions" >&2
  fi
  exit 1
}

upload() {
  local file="$1"
  local mime="$2"
  echo "PUT ${file} -> ${endpoint}/${zone}/${file}"
  storage_put "$file" "${root}/${file}" "$mime"
}

download_existing index.html
download_existing app.js
download_existing style.css
receipt=".jmodguard-publication.json"
download_existing "$receipt"
# Invalidate the one mutable receipt before any PUT, even on A -> B -> A.
# Keep the last completed asset hashes so a CSS-only retry stays independent.
bash "$(dirname "$0")/frontend_publication.sh" pending "$root" "$backup_dir/$receipt" >"$backup_dir/pending.json"
bash "$(dirname "$0")/frontend_publication.sh" complete "$root" "$backup_dir/$receipt" >"$backup_dir/complete.json"
if ! storage_put "$receipt" "$backup_dir/pending.json" "application/json"; then
  echo "cannot mark publication pending; refusing to upload assets" >&2
  exit 1
fi

# Assets first, HTML last, so a failed later PUT cannot pair new HTML with
# old JS/CSS. On upload failure, restore all three backed-up objects (or
# delete ones confirmed absent), including the file whose PUT may have
# succeeded remotely after curl reported an error. A failed Pull Zone purge
# does not roll Storage back: the new files are the intended origin, and
# reverting them cannot un-fetch edges that already missed cache.
if ! upload app.js "application/javascript; charset=utf-8"; then
  abort_upload
fi
if ! upload style.css "text/css; charset=utf-8"; then
  abort_upload
fi
if ! upload index.html "text/html; charset=utf-8"; then
  abort_upload
fi

echo "Purging Pull Zone ${pull_zone}"
purged=0
for _ in 1 2 3 4 5 6; do
  if curl -fsS -X POST "https://api.bunny.net/pullzone/${pull_zone}/purgeCache" \
    -H "AccessKey: ${key}" \
    -H "Content-Type: application/json" \
    -d '{}'; then
    purged=1
    break
  fi
  sleep "$retry_sleep"
done
if [[ "$purged" -ne 1 ]]; then
  echo "Storage already has the new frontend/. Pull Zone purge failed; CDN may keep stale or mixed cached files until a retry or dashboard purge succeeds. Publication remains pending for the next run." >&2
  exit 1
fi

# A failed/lost response here leaves a safe retry (or an already valid receipt).
if ! storage_put "$receipt" "$backup_dir/complete.json" "application/json"; then
  echo "publication receipt failed; next run will reconcile and retry" >&2
  exit 1
fi

echo "frontend/ published"
