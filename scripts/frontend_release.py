#!/usr/bin/env python3
"""Private-ops frontend release selection and live publication checks.

The operator approves API compatibility and recovery before committing the
release. This checker verifies that selected API is active, not its provenance.
Container responses contain secrets: keep them in memory and never log them.
"""
import json
import os
from pathlib import Path
import re
import sys
import urllib.error
import urllib.parse
import urllib.request

import frontend_publication as publication


def release(path):
    data = json.loads(Path(path).read_text())
    if not isinstance(data, dict) or data.get('version') != 1 or type(data.get('enabled')) is not bool:
        raise ValueError('invalid frontend release version/enabled')
    if not data['enabled']:
        return None
    for key, pattern in [('source_sha', r'[0-9a-f]{40}'), ('api_digest', r'sha256:[0-9a-f]{64}')]:
        if not isinstance(data.get(key), str) or not re.fullmatch(pattern, data[key]):
            raise ValueError('release must select a full source SHA and API digest')
    return data


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


def read(url, headers):
    request = urllib.request.Request(url, headers=headers)
    try:
        response = urllib.request.build_opener(NoRedirect).open(request, timeout=30)
    except urllib.error.HTTPError as error:
        response = error
    with response:
        return response.code, response.read()


def should_publish(source, approved, scheduled):
    zone_id = publication.configured_zone()
    # Require purge credentials before any Storage access, even on a skip.
    key = os.environ['BUNNY_API_KEY']
    if not key:
        raise ValueError('purge API key is required')
    zone = urllib.parse.quote(os.environ['BUNNY_STORAGE_ZONE'], safe='')
    endpoint = os.environ.get('BUNNY_STORAGE_ENDPOINT') or 'storage.bunnycdn.com'
    endpoint = endpoint.removeprefix('https://').rstrip('/')
    root = Path(source, 'frontend')
    desired = publication.asset_hashes(str(root))
    headers = {'AccessKey': os.environ['BUNNY_STORAGE_PASSWORD']}
    changed = set()
    for name in publication.FILES:
        code, body = read(f'https://{endpoint}/{zone}/{name}', headers)
        if code not in (200, 404):
            raise ValueError('cannot establish Storage state')
        if code == 404 or body != (root / name).read_bytes():
            changed.add(name)
    code, body = read(f'https://{endpoint}/{zone}/{publication.RECEIPT}', headers)
    if code not in (200, 404):
        raise ValueError('cannot establish publication completion')
    receipt = publication.parse(body, zone_id) if code == 200 else None
    previous = receipt['files'] if receipt else {}
    changed.update(name for name in publication.FILES if previous.get(name) != desired[name])
    complete = bool(receipt and receipt['complete'] and not changed)
    pending_api = bool(receipt and not receipt['complete'] and receipt['requires_api'])
    if not pending_api and not changed.intersection({'index.html', 'app.js'}):
        return not complete or not scheduled
    app_id = urllib.parse.quote(os.environ['JMODGUARD_API_APP_ID'], safe='')
    code, body = read(f'https://api.bunny.net/mc/apps/{app_id}', {'AccessKey': key})
    if code != 200:
        raise ValueError('cannot inspect API container')
    app = json.loads(body)
    containers = app.get('containerTemplates', []) if isinstance(app, dict) else []
    matches = [item for item in containers if isinstance(item, dict) and item.get('imageName') == 'jmodguard-api']
    if len(matches) != 1 or matches[0].get('imageDigest') != approved['api_digest'] or app.get('status', '').lower() != 'active':
        raise ValueError('API must be active with the approved digest before JS/HTML publication')
    url = os.environ['JMODGUARD_READINESS_URL']
    parsed = urllib.parse.urlsplit(url)
    if parsed.scheme != 'https' or not parsed.hostname or parsed.username or parsed.password or parsed.path != '/api/me' or parsed.query or parsed.fragment:
        raise ValueError('readiness URL must be https://<public CDN hostname>/api/me')
    code, body = read(url, {'Accept': 'application/json'})
    if code != 400 or json.loads(body) != {'error': 'not authenticated'}:
        raise ValueError('API application is not ready')
    return True


def main():
    if os.environ.get('GITHUB_REF') != 'refs/heads/main':
        raise ValueError('run production publication from ops main')
    selected = release(sys.argv[2])
    if sys.argv[1] == 'resolve':
        print(f'enabled={str(selected is not None).lower()}')
        if selected:
            print(f'source_sha={selected["source_sha"]}')
    elif sys.argv[1] == 'check' and selected:
        source = sys.argv[3]
        import subprocess
        actual = subprocess.check_output(['git', '-C', source, 'rev-parse', 'HEAD'], text=True).strip()
        if actual != selected['source_sha']:
            raise ValueError('source checkout does not match release')
        publish = should_publish(source, selected, os.environ.get('GITHUB_EVENT_NAME') == 'schedule')
        print(f'publish={str(publish).lower()}')
    else:
        raise ValueError('expected resolve or an enabled release check')


if __name__ == '__main__':
    try:
        main()
    except Exception:
        # Provider payloads and transport exceptions may contain credentials.
        print('Frontend release check failed; verify approval, configuration, Storage and API readiness.', file=sys.stderr)
        sys.exit(1)
