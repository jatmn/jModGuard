#!/usr/bin/env python3
"""Offline Stable/Nightly publication contracts. No registry, GitHub or deploy."""

import importlib.util
import json
from pathlib import Path
import tempfile
import unittest

spec = importlib.util.spec_from_file_location("release", Path(__file__).with_name("release.py"))
release = importlib.util.module_from_spec(spec)
spec.loader.exec_module(release)

SHA = "a" * 40
BOT = release.component("bot", "sha256:" + "1" * 64, 10, "ghcr.io/jatmn/jmodguard-bot")
API = release.component("api", "sha256:" + "2" * 64, 11, "ghcr.io/jatmn/jmodguard-api")
CONDUCTOR = release.component(
    "conductor", "sha256:" + "3" * 64, 12, "ghcr.io/jatmn/jmodguard-conductor"
)


def frontend(root):
    (root / "index.html").write_text("<html></html>")
    (root / "app.js").write_text("void 0;")
    (root / "style.css").write_text("body{}")
    return release.frontend_inventory(root)


class ReleaseTests(unittest.TestCase):
    def complete(self, channel="stable", release_id="v0.2.0", **extra):
        with tempfile.TemporaryDirectory() as tmp:
            components = {
                "bot": BOT,
                "api": API,
                "conductor": CONDUCTOR,
                "frontend": frontend(Path(tmp)),
            }
            components.update(extra.pop("components", {}))
            return release.build_unsigned(
                channel, release_id, extra.pop("sha", SHA), extra.pop("published_at", "2026-09-15T00:00:00Z"),
                components, extra.pop("notes", "fixture"),
            )

    def test_stable_and_nightly_channels_stay_separate(self):
        self.assertEqual(release.channel_for("tag", "v1.2.3"), "stable")
        self.assertEqual(release.channel_for("nightly", release.nightly_label(SHA)), "nightly")
        self.assertIsNone(release.channel_for("main"))
        self.assertIsNone(release.channel_for("tag", "nightly-20260915-" + SHA[:12]))
        self.assertTrue(release.stable_tag("v0.1.0"))
        self.assertFalse(release.stable_tag("nightly-20260915-" + SHA[:12]))
        self.assertEqual(
            release.release_please_ignores_nightly(["v0.1.0", "nightly-20260915-" + SHA[:12], "v0.1.0-nightly"]),
            ["v0.1.0"],
        )

    def test_complete_signed_manifest_round_trips(self):
        unsigned = self.complete()
        key_id, secret = "rp1", release.generate_hmac_key()
        signed = release.sign(unsigned, key_id, secret)
        body = release.verify(signed, {key_id: secret})
        self.assertEqual(body["format"], release.FORMAT)
        self.assertEqual(body["channel"], "stable")
        self.assertEqual(set(body["components"]), set(release.REQUIRED))

    def test_partial_or_unsigned_or_unknown_key_is_unavailable(self):
        with self.assertRaises(ValueError):
            release.build_unsigned("stable", "v0.2.0", SHA, "t", {"bot": BOT, "api": API})
        unsigned = self.complete()
        with self.assertRaises(ValueError):
            release.verify(unsigned, {"rp1": release.generate_hmac_key()})
        signed = release.sign(unsigned, "rp1", release.generate_hmac_key())
        with self.assertRaises(ValueError):
            release.verify(signed, {"rp2": release.generate_hmac_key()})
        retired = release.sign(unsigned, "old", release.generate_hmac_key())
        with self.assertRaises(ValueError):
            release.verify(retired, {})
        with self.assertRaises(ValueError):
            release.refuse_partial({"bot": BOT, "api": API})

    def test_interrupted_staging_promotes_only_after_complete_write(self):
        unsigned = self.complete()
        with tempfile.TemporaryDirectory() as tmp:
            path = release.stage_write(tmp, unsigned["release_id"], unsigned)
            self.assertTrue(path.exists())
            self.assertFalse(list(Path(tmp).glob(".*.staging.json")))
            loaded = json.loads(path.read_text())
            self.assertEqual(loaded["release_id"], "v0.2.0")

    def test_index_retention_keeps_channels_independent(self):
        rows = []
        for index in range(5):
            rows.append({"release_id": f"v0.0.{index}", "channel": "stable", "published_at": f"2026-09-0{index+1}"})
            rows.append({
                "release_id": f"nightly-2026090{index+1}-" + SHA[:12],
                "channel": "nightly",
                "published_at": f"2026-09-1{index}",
            })
        index = release.index_releases(rows, retain=2)
        channels = {row["channel"] for row in index["releases"]}
        self.assertEqual(channels, {"stable", "nightly"})
        self.assertEqual(len([row for row in index["releases"] if row["channel"] == "stable"]), 2)
        self.assertEqual(len([row for row in index["releases"] if row["channel"] == "nightly"]), 2)

    def test_nightly_cannot_reuse_stable_id_or_latest_pointer(self):
        label = release.nightly_label(SHA, when=0)
        self.assertTrue(release.nightly_tag(label))
        with self.assertRaises(ValueError):
            self.complete(channel="nightly", release_id="v0.2.0")
        with self.assertRaises(ValueError):
            self.complete(channel="stable", release_id=label)

    def test_trust_rotation_accepts_current_key_and_ignores_retired(self):
        live = release.generate_hmac_key()
        old = release.generate_hmac_key()
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "trust.json"
            path.write_text(json.dumps({
                "format": release.TRUST_FORMAT,
                "keys": [
                    {"id": "old", "alg": "hmac-sha256", "secret": old, "retired": True},
                    {"id": "live", "alg": "hmac-sha256", "secret": live},
                ],
            }))
            keys = release.load_trust(path)
        self.assertEqual(set(keys), {"live"})
        signed = release.sign(self.complete(), "live", live)
        release.verify(signed, keys)
        with self.assertRaises(ValueError):
            release.verify(release.sign(self.complete(), "old", old), keys)

    def test_release_please_config_is_stable_only(self):
        root = Path(__file__).resolve().parents[1]
        config = release.load_release_please(root / "release-please-config.json")
        self.assertTrue(config["include-v-in-tag"])
        manifest = json.loads((root / ".release-please-manifest.json").read_text())
        self.assertEqual(manifest["."], "0.1.0")
        self.assertEqual(release.release_please_ignores_nightly(["nightly-20260915-" + SHA[:12]]), [])

    def test_finalize_refuses_partial_then_writes_complete_index(self):
        key_id, secret = "rp1", release.generate_hmac_key()
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            frontend_root = root / "frontend"
            frontend_root.mkdir()
            components = {"bot": BOT, "api": API, "conductor": CONDUCTOR, "frontend": frontend(frontend_root)}
            with self.assertRaises(ValueError):
                release.finalize("stable", "v0.2.0", SHA, {"bot": BOT}, "", key_id, secret, root / "out")
            path = release.finalize("stable", "v0.2.0", SHA, components, "notes", key_id, secret, root / "out")
            index = json.loads((root / "out" / "index.json").read_text())
            self.assertEqual(index["format"], release.INDEX_FORMAT)
            self.assertEqual(index["releases"][0]["release_id"], "v0.2.0")
            release.verify(json.loads(path.read_text()), {key_id: secret})


if __name__ == "__main__":
    unittest.main()
