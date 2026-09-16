#!/usr/bin/env python3
"""Public-source install docs must not require the private ops repository."""
from __future__ import annotations

import importlib.util
import os
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


class PublicInstallDocs(unittest.TestCase):
    def read(self, relative: str) -> str:
        return (ROOT / relative).read_text(encoding="utf-8")

    def test_license_is_polyform_noncommercial(self):
        license_text = self.read("LICENSE.md")
        self.assertIn("PolyForm Noncommercial License 1.0.0", license_text)
        cargo = self.read("Cargo.toml")
        self.assertIn("LicenseRef-PolyForm-Noncommercial-1.0.0", cargo)

    def test_install_guide_is_public_and_lists_deferred_checks(self):
        install = self.read("docs/INSTALL.md")
        self.assertIn("without the private", install)
        self.assertIn("ops repo", install)
        self.assertIn("not required", install)
        self.assertIn("Still-deferred live provider checks", install)
        self.assertIn("Live Bunny Database", install)
        self.assertIn("recover-update", install)
        self.assertIn("LICENSE.md", install)

    def test_aws_path_does_not_promise_rds(self):
        aws = self.read("docs/DEPLOY-AWS.md")
        self.assertIn("There is **no** RDS", aws)
        self.assertIn("Not RDS/Postgres", aws)
        self.assertIn("EC2", aws)
        self.assertIn("libSQL", aws)
        self.assertNotIn("create an RDS instance", aws.lower())
        self.assertNotIn(
            "jmodguard-bot`, `jmodguard-api`, `jmodguard-conductor`",
            aws,
        )

    def test_public_artifact_wording_matches_install(self):
        install = self.read("docs/INSTALL.md")
        self.assertIn("does not upload a separate frontend", install)
        bunny = self.read("docs/DEPLOY-BUNNY.md")
        self.assertNotIn("Buildkite frontend artifact", bunny)
        webui = self.read("docs/WEBUI.md")
        self.assertNotIn("matching Buildkite artifact", webui)
        self.assertIn("does not upload a separate frontend", webui)
        overhaul = self.read("docs/OVERHAUL.md")
        self.assertNotIn(
            "or Buildkite artifacts is the required path",
            overhaul,
        )
        self.assertIn("does not upload a separate frontend", overhaul)
        install_rs = self.read("crates/conductor/src/install.rs")
        self.assertNotIn(
            "Buildkite already publishes public bot/API/conductor/frontend artifacts",
            install_rs,
        )
        self.assertIn("Nightly/Stable/manual", install_rs)

    def test_bunny_and_webui_do_not_require_private_ops(self):
        bunny = self.read("docs/DEPLOY-BUNNY.md")
        self.assertNotIn(
            "The UI files are published by **GitHub Actions in your private ops repo**.",
            bunny,
        )
        self.assertIn("optional", bunny.lower())
        self.assertIn("not required", bunny.lower())
        webui = self.read("docs/WEBUI.md")
        self.assertIn("not required", webui.lower())
        example = self.read("docs/examples/private-ops/README.md")
        self.assertIn("optional", example.lower())

    def test_cost_formulas_are_measured_not_invented(self):
        cost = self.read("docs/COST.md")
        self.assertIn("local measurements", cost.lower())
        self.assertIn("does not invent", cost.lower())
        self.assertIn("256 MiB", cost)
        self.assertIn("list prices", cost.lower())

    def test_validate_contract_is_check_not_apply_across_operator_surfaces(self):
        surfaces = [
            "docs/INSTALL.md",
            "docs/OVERHAUL.md",
            "docs/CONDUCTOR.md",
            "docs/DEPLOY-AWS.md",
            "docs/DEPLOY-BUNNY.md",
            "docs/WEBUI.md",
            "docs/SETUP.md",
            "AGENTS.md",
            "README.md",
            "crates/conductor/src/install.rs",
            "crates/conductor/frontend/app.js",
        ]
        for relative in surfaces:
            with self.subTest(relative):
                text = self.read(relative)
                self.assertNotIn("Install plan accepted", text)
                self.assertNotIn(
                    "topologies, disjoint private roots, scope-limited URLs",
                    text,
                )
                self.assertNotIn(
                    "The conductor checks topology, distinct private roots",
                    text,
                )
        install = self.read("docs/INSTALL.md")
        self.assertIn("does **not** apply process configuration", install)
        self.assertIn("without requiring those local", install)
        self.assertIn("`0700`", install)
        overhaul = self.read("docs/OVERHAUL.md")
        self.assertIn("does not apply process configuration", overhaul)
        self.assertIn("without requiring those local paths", overhaul)
        conductor = self.read("docs/CONDUCTOR.md")
        self.assertIn("without applying conductor", conductor)
        app = self.read("crates/conductor/frontend/app.js")
        self.assertIn("Install plan validated (not applied)", app)
        steps = self.read("crates/conductor/src/install.rs")
        self.assertIn("does not apply process configuration", steps)

    def test_readme_points_at_public_install(self):
        readme = self.read("README.md")
        self.assertIn("docs/INSTALL.md", readme)
        self.assertIn("docs/DEPLOY-AWS.md", readme)
        self.assertNotIn("Point-and-click install remains later work", readme)

    def test_install_local_honors_custom_conductor_bin_path(self):
        script = ROOT / "scripts/install-local.py"
        spec = importlib.util.spec_from_file_location("install_local", script)
        self.assertIsNotNone(spec and spec.loader)
        mod = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(mod)
        with tempfile.TemporaryDirectory() as tmp:
            custom = Path(tmp) / "jmodguard-conductor-custom"
            custom.write_bytes(b"")
            with self.subTest("custom basename"):
                os.environ["JMODGUARD_CONDUCTOR_BIN"] = str(custom)
                try:
                    self.assertEqual(mod.conductor_bin(), custom)
                finally:
                    os.environ.pop("JMODGUARD_CONDUCTOR_BIN", None)


if __name__ == "__main__":
    unittest.main()
