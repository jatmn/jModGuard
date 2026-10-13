#!/usr/bin/env bash
# Disposable local libSQL fixtures; never use operator databases.
set -euo pipefail
npm ci --ignore-scripts --prefix tools/webui
npm run build --prefix tools/webui
cargo fetch --locked
for TEST_TARGET in remote_database_smoke guild_store_smoke bunny_evidence; do
  NAME="jmodguard-ci-${RANDOM}-$$-${TEST_TARGET}"
  trap 'docker rm -f "$NAME"' EXIT
  docker run -d --name "$NAME" -p 127.0.0.1::8080 \
    ghcr.io/tursodatabase/libsql-server@sha256:1bc51611928ccc51229bdc40ca0defcb915907f8f9f6a664b43529de0ca45fab
  HOST_PORT=$(docker port "$NAME" 8080/tcp | tail -1 | cut -d: -f2)
  if [ -z "$HOST_PORT" ]; then
    echo "failed to resolve host port for $NAME" >&2
    exit 1
  fi
  export JMODGUARD_REMOTE_TEST_URL=http://127.0.0.1:${HOST_PORT}
  export JMODGUARD_REMOTE_TEST_TOKEN=local-ci-token
  ready=0
  i=0
  while [ "$i" -lt 30 ]; do
    if curl -fsS -o /dev/null --connect-timeout 1 --max-time 1 "$JMODGUARD_REMOTE_TEST_URL/health"; then
      ready=1
      break
    fi
    i=$((i + 1))
    sleep 1
  done
  if [ "$ready" -ne 1 ]; then
    echo "$NAME did not become healthy after 30 attempts" >&2
    exit 1
  fi
  if [ "$TEST_TARGET" = bunny_evidence ]; then
    cargo test --locked -p jmodguard --lib bunny_evidence::tests::remote_bunny_journal_recovers_commits -- --ignored --exact --nocapture
  else
    cargo test --locked -p jmodguard-integration-tests --test "$TEST_TARGET" -- --ignored --nocapture
  fi
  docker rm -f "$NAME"
  trap - EXIT
done
