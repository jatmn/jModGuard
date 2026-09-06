#!/usr/bin/env python3
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

import frontend_release as gate


class ReleaseTests(unittest.TestCase):
    def setUp(self):
        self.scratch = tempfile.TemporaryDirectory(dir=os.environ.get('TMPDIR'))
        self.addCleanup(self.scratch.cleanup)
        self.root = Path(self.scratch.name)
        (self.root / 'frontend').mkdir()
        for name in gate.publication.FILES:
            (self.root / 'frontend' / name).write_text(name)
        self.approved = {'version': 1, 'enabled': True, 'source_sha': 'a' * 40, 'api_digest': 'sha256:' + 'b' * 64}
        self.env = patch.dict(os.environ, {
            'BUNNY_PULL_ZONE_ID': '123', 'BUNNY_API_KEY': 'fake',
            'BUNNY_STORAGE_ZONE': 'zone', 'BUNNY_STORAGE_PASSWORD': 'fake',
            'BUNNY_STORAGE_ENDPOINT': 'storage.test', 'JMODGUARD_API_APP_ID': 'app',
            'JMODGUARD_READINESS_URL': 'https://public.test/api/me',
        })
        self.env.start()
        self.addCleanup(self.env.stop)
        self.receipt = {'version': 1, 'complete': True, 'pull_zone_id': '123',
                        'files': gate.publication.asset_hashes(str(self.root / 'frontend'))}
        self.stored = {name: (self.root / 'frontend' / name).read_bytes() for name in gate.publication.FILES}
        self.digest = self.approved['api_digest']
        self.ready = (400, b'{"error":"not authenticated"}')
        self.calls = []

    def read(self, url, headers):
        self.calls.append(url)
        if url.startswith('https://api.bunny.net/'):
            return 200, json.dumps({'status': 'active', 'containerTemplates': [
                {'imageName': 'jmodguard-api', 'imageDigest': self.digest}]}).encode()
        if url.startswith('https://public.test/'):
            return self.ready
        name = url.rsplit('/', 1)[1]
        if name == gate.publication.RECEIPT:
            return 200, json.dumps(self.receipt).encode()
        return 200, self.stored[name]

    def check(self, scheduled=True):
        with patch.object(gate, 'read', side_effect=self.read):
            return gate.should_publish(str(self.root), self.approved, scheduled)

    def test_release_rejects_floating_refs_and_rebuilt_digest_omission(self):
        path = self.root / 'release.json'
        for field, value in [('source_sha', 'main'), ('source_sha', 'abc123'), ('api_digest', 'latest'), ('enabled', 'true')]:
            with self.subTest(field=field, value=value):
                path.write_text(json.dumps(self.approved | {field: value}))
                with self.assertRaises(ValueError):
                    gate.release(path)
        path.write_text(json.dumps(self.approved | {'enabled': False}))
        self.assertIsNone(gate.release(path))

    def test_schedule_skips_only_completed_matching_publication(self):
        self.assertFalse(self.check())
        self.assertTrue(self.check(scheduled=False))
        self.receipt['complete'] = False
        self.assertTrue(self.check())

    def test_css_only_can_publish_without_api(self):
        (self.root / 'frontend/style.css').write_text('new CSS')
        self.assertTrue(self.check())
        self.assertFalse(any('api.bunny.net' in url for url in self.calls))

    def test_js_requires_exact_approved_digest_and_application_readiness(self):
        (self.root / 'frontend/app.js').write_text('new JS')
        self.digest = 'sha256:' + 'c' * 64
        with self.assertRaises(ValueError):
            self.check()
        self.digest = self.approved['api_digest']
        for response in [(503, b'{"error":"not ready"}'), (200, b'ok'), (400, b'<html>error</html>')]:
            self.ready = response
            with self.assertRaises(ValueError):
                self.check()
        self.ready = (400, b'{"error":"not authenticated"}')
        self.assertTrue(self.check())

    def test_previous_tree_receipt_cannot_skip_pending_purge(self):
        # Origin has reverted to A, but last completed publication was B.
        self.receipt['files']['app.js'] = 'c' * 64
        self.receipt['complete'] = False
        self.assertTrue(self.check())
        self.assertTrue(any('api.bunny.net' in url for url in self.calls))

    def test_matching_pending_js_repair_rechecks_digest_and_readiness(self):
        self.receipt.update(complete=False, requires_api=True)
        self.digest = 'sha256:' + 'c' * 64
        with self.assertRaises(ValueError):
            self.check()
        self.digest = self.approved['api_digest']
        self.ready = (503, b'{"error":"not ready"}')
        with self.assertRaises(ValueError):
            self.check()
        self.ready = (400, b'{"error":"not authenticated"}')
        self.assertTrue(self.check())

    def test_legacy_pending_is_conservative_but_known_css_retry_is_independent(self):
        self.receipt['complete'] = False
        self.digest = 'sha256:' + 'c' * 64
        with self.assertRaises(ValueError):
            self.check()
        self.receipt['requires_api'] = False
        self.calls.clear()
        self.assertTrue(self.check())
        self.assertFalse(any('api.bunny.net' in url for url in self.calls))

    def test_receipt_carries_actual_js_repair_intent_until_completion(self):
        backups = self.root / 'backups'
        backups.mkdir()
        for name in gate.publication.FILES:
            (backups / name).write_bytes(self.stored[name])
        previous = backups / gate.publication.RECEIPT
        previous.write_text(json.dumps(self.receipt))
        root = str(self.root / 'frontend')
        (self.root / 'frontend/style.css').write_text('new CSS')
        pending = gate.publication.make_receipt('pending', root, str(previous), '123')
        self.assertFalse(pending['requires_api'])
        # JS drift repair has the same target/last-completed hashes.
        (backups / 'app.js').write_text('uncompleted JS')
        pending = gate.publication.make_receipt('pending', root, str(previous), '123')
        self.assertTrue(pending['requires_api'])
        self.assertEqual(pending['files']['app.js'], self.receipt['files']['app.js'])
        previous.write_text(json.dumps(pending))
        (backups / 'app.js').write_bytes(self.stored['app.js'])
        retry = gate.publication.make_receipt('pending', root, str(previous), '123')
        self.assertTrue(retry['requires_api'])
        completed = gate.publication.make_receipt('complete', root, str(previous), '123')
        self.assertTrue(completed['complete'])
        self.assertFalse(completed['requires_api'])

    def test_invalid_zone_and_storage_errors_fail_closed(self):
        with patch.dict(os.environ, {'BUNNY_PULL_ZONE_ID': ''}):
            with self.assertRaises(ValueError):
                self.check()
        self.assertEqual(self.calls, [])
        with patch.object(gate, 'read', return_value=(500, b'error')):
            with self.assertRaises(ValueError):
                gate.should_publish(str(self.root), self.approved, True)

    def test_cli_rejects_non_main_and_mismatched_checkout_before_network(self):
        path = self.root / 'release.json'
        path.write_text(json.dumps(self.approved))
        script = str(Path(gate.__file__).resolve())
        env = os.environ.copy()
        env['GITHUB_REF'] = 'refs/heads/feature'
        result = subprocess.run([sys.executable, script, 'resolve', str(path)], env=env, capture_output=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(result.stdout, b'')
        env['GITHUB_REF'] = 'refs/heads/main'
        result = subprocess.run([sys.executable, script, 'resolve', str(path)], env=env, capture_output=True)
        self.assertEqual(result.returncode, 0)
        self.assertIn(('source_sha=' + self.approved['source_sha']).encode(), result.stdout)
        # A different git checkout fails before any credentials are read.
        with patch.object(gate, 'read', side_effect=AssertionError('unexpected network')), patch.dict(os.environ, env), patch.object(sys, 'argv', [script, 'check', str(path), str(self.root)]), patch('subprocess.check_output', return_value='c' * 40):
            with self.assertRaisesRegex(ValueError, 'checkout'):
                gate.main()


if __name__ == '__main__':
    unittest.main()
