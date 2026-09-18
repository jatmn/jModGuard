#!/usr/bin/env python3
"""Guard Dependabot YAML and the operator-copy contract for version updates."""
from pathlib import Path
import unittest

ROOT = Path(__file__).resolve().parents[1]


def read(relative):
    return (ROOT / relative).read_text(encoding="utf-8")


class DependabotConfigTests(unittest.TestCase):
    def setUp(self):
        self.yaml = read(".github/dependabot.yml")
        self.development = read("development.md")
        self.buildkite = read("docs/BUILDKITE.md")
        self.deploy = read("docs/DEPLOY-BUNNY.md")
        self.agents = read("AGENTS.md")
        self.preflight = read("scripts/ci-preflight.sh")

    def test_cargo_schedule_stays_weekly_workspace_updates(self):
        for needle in (
            "version: 2",
            "package-ecosystem: cargo",
            'directory: "/"',
            "interval: weekly",
            "open-pull-requests-limit: 5",
            "cargo-minor-and-patch",
            "- minor",
            "- patch",
            "dependency-name: libsql-sqlite3-parser",
        ):
            with self.subTest(needle=needle):
                self.assertIn(needle, self.yaml)
        self.assertNotIn("package-ecosystem: github-actions", self.yaml)
        self.assertNotIn("package-ecosystem: docker", self.yaml)
        self.assertNotIn("${{", self.yaml)
        self.assertFalse((ROOT / ".github/workflows").exists())

    def test_yaml_documents_self_hosted_runner_trap(self):
        for needle in (
            "Dependabot on self-hosted runners",
            "`dependabot`",
            "no GitHub Actions self-hosted runners",
            "24 hours",
            "GitHub Packages",
        ):
            with self.subTest(needle=needle):
                self.assertIn(needle, self.yaml)

    def test_operator_copy_names_actions_queue_not_missing_schedule(self):
        for path, text in (
            ("development.md", self.development),
            ("docs/BUILDKITE.md", self.buildkite),
            ("docs/DEPLOY-BUNNY.md", self.deploy),
            ("AGENTS.md", self.agents),
        ):
            with self.subTest(path=path):
                self.assertIn("Dependabot", text)
        self.assertIn("Actions → Dependabot Updates", self.development)
        self.assertIn("Dependabot on self-hosted runners", self.development)
        self.assertIn("24 hours", self.development)
        self.assertIn("idle", self.development)
        self.assertIn("no GitHub Actions self-hosted runners", self.development)
        self.assertIn("python3 scripts/dependabot-config-test.py", self.development)
        self.assertIn("Actions → Dependabot Updates", self.buildkite)
        self.assertIn("Dependabot on self-hosted", self.buildkite)
        self.assertIn("no registered GitHub Actions self-hosted runners", self.buildkite)
        self.assertNotIn("No Actions jobs should start for this repo.", self.buildkite)
        self.assertIn("no GitHub Actions CI or", self.deploy)
        self.assertIn("Dependabot on self-hosted runners", self.deploy)
        self.assertNotIn("still use GitHub-hosted", self.deploy)
        self.assertNotIn("This repository has no GitHub Actions jobs", self.deploy)
        self.assertIn("python3 scripts/dependabot-config-test.py", self.agents)
        self.assertIn("Dependabot on self-hosted runners", self.agents)
        self.assertIn("24 hours", self.agents)
        self.assertIn("no GitHub Actions self-hosted", self.agents)
        self.assertIn("python3 scripts/dependabot-config-test.py", self.preflight)
        self.assertIn("GitHub Actions workflows and GitHub-hosted CI runners are forbidden", self.preflight)
        self.assertIn("Dependabot Updates dynamic workflows are not CI", self.preflight)


if __name__ == "__main__":
    unittest.main()
