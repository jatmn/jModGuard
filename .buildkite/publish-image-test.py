#!/usr/bin/env python3
"""Offline publication contract tests; never log in, push, or contact Buildkite."""

import importlib.util
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch


spec = importlib.util.spec_from_file_location("publisher", Path(__file__).with_name("publish-image.py"))
publisher = importlib.util.module_from_spec(spec)
spec.loader.exec_module(publisher)
HEAD = "a" * 40
BASE = "b" * 40
MAIN = {"BUILDKITE": "true", "BUILDKITE_SOURCE": "webhook", "BUILDKITE_BRANCH": "main",
        "BUILDKITE_COMMIT": HEAD, "BUILDKITE_PULL_REQUEST": "false", "BUILDKITE_RETRY_COUNT": "0"}


class PublicationTests(unittest.TestCase):
    def test_only_main_push_tag_or_explicit_manual_can_publish(self):
        self.assertEqual(publisher.request(MAIN), ("main", ""))
        self.assertEqual(publisher.request(MAIN | {"BUILDKITE_TAG": "v1", "BUILDKITE_BRANCH": "v1"}),
                         ("tag", "refs/tags/v1"))
        for changes in ({"BUILDKITE_PULL_REQUEST": "7"}, {"BUILDKITE_BRANCH": "feature"},
                        {"BUILDKITE_SOURCE": "ui"}, {"BUILDKITE_SOURCE": "schedule"},
                        {"BUILDKITE_GITHUB_EVENT": "pull_request"},
                        {"BUILDKITE_GITHUB_EVENT": "release"}):
            with self.subTest(changes=changes):
                self.assertIsNone(publisher.request(MAIN | changes))
        with self.assertRaises(ValueError):
            publisher.request({})
        # Leftover manual source on a webhook must not block automatic publication.
        self.assertEqual(publisher.request(MAIN | {"JMODGUARD_PUBLISH_SOURCE": HEAD}), ("main", ""))
        self.assertEqual(
            publisher.request(MAIN | {"BUILDKITE_TAG": "v1", "BUILDKITE_BRANCH": "v1",
                                       "JMODGUARD_PUBLISH_SOURCE": HEAD}),
            ("tag", "refs/tags/v1"),
        )

    def test_manual_requires_main_and_explicit_immutable_or_tag_source(self):
        env = MAIN | {"BUILDKITE_SOURCE": "ui"}
        for ref in (HEAD, "refs/tags/v1", "refs/tags/release/v1+meta"):
            self.assertEqual(publisher.request(env | {"JMODGUARD_PUBLISH_SOURCE": ref}), ("manual", ref))
        for ref in ("main", "HEAD", "abc123", "refs/heads/main", "v1", "-oops"):
            with self.assertRaises(ValueError):
                publisher.request(env | {"JMODGUARD_PUBLISH_SOURCE": ref})
        with self.assertRaises(ValueError):
            publisher.request(env | {"BUILDKITE_BRANCH": "feature", "JMODGUARD_PUBLISH_SOURCE": HEAD})

    def test_run_allowlists_programs_and_cannot_enable_shell(self):
        with patch.object(publisher.subprocess, "run",
                           return_value=subprocess.CompletedProcess(["git"], 0, "")) as mocked:
            publisher.run("git", "status", shell=True)
            argv, kwargs = mocked.call_args
            self.assertEqual(argv[0][0], "git")
            self.assertFalse(kwargs["shell"])
        with self.assertRaises(ValueError):
            publisher.run("bash", "-c", "true")

    def test_pr_and_test_only_builds_do_not_read_secrets_or_run_commands(self):
        for env in (MAIN | {"BUILDKITE_PULL_REQUEST": "1"}, MAIN | {"BUILDKITE_SOURCE": "ui"}):
            with patch.dict(os.environ, env, clear=True), patch.object(publisher, "run") as run:
                publisher.prepare()
                publisher.publish("bot")
                run.assert_not_called()

    def test_moving_tag_is_resolved_once_and_plan_is_atomic(self):
        with patch.dict(os.environ, MAIN | {"BUILDKITE_SOURCE": "ui", "JMODGUARD_PUBLISH_SOURCE": "refs/tags/v1"}, clear=True), \
             patch.object(publisher, "git", side_effect=[HEAD, ""]), \
             patch.object(publisher, "resolve_source", return_value=(BASE, "v1")) as resolve, \
             patch.object(publisher, "run", return_value=subprocess.CompletedProcess([], 100, "")) as run:
            publisher.prepare()
        resolve.assert_called_once_with("manual", "refs/tags/v1", HEAD)
        self.assertEqual(run.call_count, 2)
        args = run.call_args.args
        self.assertEqual(args[:4], ("buildkite-agent", "meta-data", "set", "image-publication"))
        self.assertEqual(json.loads(args[4]), {"mode": "manual", "ref": "refs/tags/v1",
                                             "commit": HEAD, "sha": BASE, "tag": "v1"})

    def test_prepare_retry_preserves_plan_even_if_tag_moved(self):
        selection = ("manual", "refs/tags/v1", HEAD)
        with patch.object(publisher, "checked_request", return_value=selection), \
             patch.object(publisher, "run", return_value=subprocess.CompletedProcess([], 0, "")), \
             patch.object(publisher, "read_plan", return_value=(BASE, "v1")) as read, \
             patch.object(publisher, "resolve_source") as resolve:
            publisher.prepare()
        read.assert_called_once_with(selection)
        resolve.assert_not_called()

    def test_bad_checkout_stops_before_registry_access(self):
        for outputs in ([BASE], [HEAD, " M src/lib.rs"]):
            with patch.dict(os.environ, MAIN, clear=True), patch.object(publisher, "git", side_effect=outputs), \
                 patch.object(publisher, "run") as run, self.assertRaises(ValueError):
                publisher.publish("bot")
            run.assert_not_called()

    def exercise_publish(self, *, changed=True, fail_build=False, plan=None, mode="main", source_token=""):
        calls = []
        env = MAIN | {"GHCR_USERNAME": "publisher", "GHCR_TOKEN": "test-secret", "GITHUB_TOKEN": source_token}
        ref = ""
        sha = HEAD
        if mode == "manual":
            ref = "refs/tags/v1"
            sha = BASE
            env |= {"BUILDKITE_SOURCE": "ui", "JMODGUARD_PUBLISH_SOURCE": ref}
        plan = plan if plan is not None else {"mode": mode, "ref": ref, "commit": HEAD, "sha": sha, "tag": ""}

        def command(*args, **kwargs):
            calls.append((args, kwargs))
            if args[:3] == ("buildkite-agent", "meta-data", "get"):
                return subprocess.CompletedProcess(args, 0, json.dumps(plan))
            if args[:3] == ("docker", "buildx", "build") and fail_build:
                raise subprocess.CalledProcessError(1, args)
            return subprocess.CompletedProcess(args, 0, "")

        with tempfile.TemporaryDirectory(dir=os.environ.get("TMPDIR")) as scratch:
            env["TMPDIR"] = scratch
            with patch.dict(os.environ, env, clear=True), patch.object(publisher, "git", side_effect=[HEAD, ""]), \
                 patch.object(publisher, "run", side_effect=command), \
                 patch.object(publisher, "changed_since_publication", return_value=changed) as compare, \
                 patch.object(publisher, "resolve_source", return_value=(sha, "")) as resolve:
                try:
                    publisher.publish("api")
                except subprocess.CalledProcessError:
                    if not fail_build:
                        raise
                self.assertEqual(list(Path(scratch).iterdir()), [])
                if mode == "manual":
                    resolve.assert_called_once_with("manual", BASE, HEAD)
                    compare.assert_not_called()
        return calls

    def test_unchanged_main_skips_compile_and_publication(self):
        calls = self.exercise_publish(changed=False)
        self.assertFalse(any(args[:3] == ("docker", "buildx", "build") for args, _ in calls))
        self.assertFalse(any(args[:2] == ("git", "worktree") for args, _ in calls))
        self.assertFalse(any(args[:3] == ("buildkite-agent", "secret", "get") for args, _ in calls))
        self.assertFalse(any(args[:2] == ("docker", "login") for args, _ in calls))
        self.assertFalse(any(args[:3] == ("buildkite-agent", "redactor", "add") for args, _ in calls))

    def test_unchanged_main_skips_without_registry_credentials(self):
        calls = []
        env = MAIN | {"GITHUB_TOKEN": ""}
        plan = {"mode": "main", "ref": "", "commit": HEAD, "sha": HEAD, "tag": ""}

        def command(*args, **kwargs):
            calls.append((args, kwargs))
            if args[:3] == ("buildkite-agent", "meta-data", "get"):
                return subprocess.CompletedProcess(args, 0, json.dumps(plan))
            raise AssertionError("skip path must not run " + " ".join(args))

        with tempfile.TemporaryDirectory(dir=os.environ.get("TMPDIR")) as scratch:
            env["TMPDIR"] = scratch
            with patch.dict(os.environ, env, clear=True), patch.object(publisher, "git", side_effect=[HEAD, ""]), \
                 patch.object(publisher, "run", side_effect=command), \
                 patch.object(publisher, "changed_since_publication", return_value=False):
                publisher.publish("api")
        self.assertEqual(len(calls), 1)
        self.assertEqual(calls[0][0][:3], ("buildkite-agent", "meta-data", "get"))

    def test_publish_flags_credentials_and_cleanup(self):
        calls = self.exercise_publish()
        args, options = next(c for c in calls if c[0][:3] == ("docker", "buildx", "build"))
        self.assertIn("--provenance=false", args)
        self.assertIn("--sbom=false", args)
        self.assertIn("linux/amd64", args)
        self.assertIn("ghcr.io/jatmn/jmodguard-api:" + HEAD, args)
        self.assertIn("ghcr.io/jatmn/jmodguard-api:latest", args)
        self.assertEqual(options["env"]["GITHUB_TOKEN"], "test-secret")
        self.assertTrue(any(a[:3] == ("git", "worktree", "remove") for a, _ in calls))
        self.assertTrue(all("test-secret" not in str(a) for a, _ in calls))
        login = next(c for c in calls if c[0][:2] == ("docker", "login"))
        self.assertEqual(login[1]["input"], "test-secret")
        self.assertFalse(Path(login[1]["env"]["DOCKER_CONFIG"]).exists())

    def test_build_failure_cleans_checkout_and_registry_login(self):
        calls = self.exercise_publish(fail_build=True)
        self.assertTrue(any(a[:3] == ("git", "worktree", "remove") for a, _ in calls))

    def test_source_credential_is_separate_and_literal_mapping_falls_back(self):
        for token, expected in (("read-only-secret", "read-only-secret"), ("$GITHUB_TOKEN", "test-secret")):
            calls = self.exercise_publish(source_token=token)
            _, options = next(c for c in calls if c[0][:3] == ("docker", "buildx", "build"))
            self.assertEqual(options["env"]["GITHUB_TOKEN"], expected)
            redactor = next(c for c in calls if c[0][:3] == ("buildkite-agent", "redactor", "add"))
            self.assertEqual(json.loads(redactor[1]["input"])["GITHUB_TOKEN"], expected)

    def test_manual_fetches_planned_sha_on_each_agent_and_never_skips(self):
        calls = self.exercise_publish(mode="manual", changed=False)
        args, _ = next(c for c in calls if c[0][:3] == ("docker", "buildx", "build"))
        self.assertIn("ghcr.io/jatmn/jmodguard-api:" + BASE, args)

    def test_invalid_plan_is_rejected(self):
        for plan in ([], {}, {"mode": "main", "ref": "", "commit": HEAD, "sha": BASE, "tag": ""}):
            with self.assertRaises(ValueError):
                self.exercise_publish(plan=plan)


class GitSourceTests(unittest.TestCase):
    def setUp(self):
        self.scratch = tempfile.TemporaryDirectory(dir=os.environ.get("TMPDIR"))
        self.addCleanup(self.scratch.cleanup)
        self.previous_cwd = Path.cwd()
        os.chdir(self.scratch.name)
        self.addCleanup(os.chdir, self.previous_cwd)
        publisher.git("init", "-b", "main")
        publisher.git("config", "user.name", "Fixture")
        publisher.git("config", "user.email", "fixture@example.invalid")
        Path("src").mkdir()
        Path("src/lib.rs").write_text("old")
        publisher.git("add", ".")
        publisher.git("commit", "-m", "base")
        self.base = publisher.git("rev-parse", "HEAD")
        publisher.git("tag", "-a", "release/v1+meta", "-m", "release")
        publisher.git("remote", "add", "origin", self.scratch.name)

    def commit(self, path):
        Path(path).parent.mkdir(parents=True, exist_ok=True)
        Path(path).write_text("changed")
        publisher.git("add", ".")
        publisher.git("commit", "-m", "change")
        return publisher.git("rev-parse", "HEAD")

    def test_annotated_tag_and_full_sha_resolution(self):
        head = self.commit("frontend/style.css")
        self.assertEqual(publisher.resolve_source("manual", "refs/tags/release/v1+meta", head),
                         (self.base, "release-v1-meta"))
        self.assertEqual(publisher.resolve_source("manual", self.base, head), (self.base, ""))
        with self.assertRaises(ValueError):
            publisher.resolve_source("tag", "refs/tags/release/v1+meta", head)
        with self.assertRaises(subprocess.CalledProcessError):
            publisher.resolve_source("manual", "refs/tags/../bad", head)

    def test_diff_spans_all_commits_since_last_image(self):
        head = self.commit("frontend/style.css")
        config = {"config": {"Labels": {"org.opencontainers.image.revision": self.base,
                                        "org.opencontainers.image.source": publisher.REPOSITORY}}}
        real_run = publisher.run

        def command(*args, **kwargs):
            if args[0] == "docker":
                if "--raw" in args:
                    self.assertIn(args[4], ("image:latest", "image:" + self.base))
                    return subprocess.CompletedProcess(args, 0, '{"schemaVersion":2}')
                return subprocess.CompletedProcess(args, 0, json.dumps(config))
            return real_run(*args, **kwargs)

        with patch.object(publisher, "run", side_effect=command):
            self.assertFalse(publisher.changed_since_publication("bot", head, "image", {}))
            self.commit("src/lib.rs")
            head = self.commit("docs/new.md")
            self.assertTrue(publisher.changed_since_publication("bot", head, "image", {}))

    def test_service_package_changes_trigger_both_copied_build_contexts(self):
        real_run = publisher.run
        for path in ("crates/bot/src/main.rs", "crates/api/src/web/api.rs",
                     "crates/runtime/src/health.rs", "migrations/guild/0001_identity.sql"):
            previous = publisher.git("rev-parse", "HEAD")
            head = self.commit(path)
            config = {"config": {"Labels": {"org.opencontainers.image.revision": previous,
                                            "org.opencontainers.image.source": publisher.REPOSITORY}}}

            def command(*args, **kwargs):
                if args[0] == "docker":
                    return subprocess.CompletedProcess(args, 0, json.dumps(config))
                return real_run(*args, **kwargs)

            with self.subTest(path=path), patch.object(publisher, "run", side_effect=command):
                for component in ("bot", "api"):
                    self.assertTrue(publisher.changed_since_publication(component, head, "image", {}))

    def test_unknown_registry_history_rebuilds(self):
        for config in ({}, {"config": {"Labels": {"org.opencontainers.image.revision": "c" * 40,
                                                  "org.opencontainers.image.source": publisher.REPOSITORY}}}):
            with patch.object(publisher, "run", return_value=subprocess.CompletedProcess([], 0, json.dumps(config))):
                # Unknown objects must not be mistaken for an empty diff.
                with patch.object(publisher, "git", side_effect=subprocess.CalledProcessError(128, ["git"])):
                    self.assertTrue(publisher.changed_since_publication("bot", self.base, "image", {}))

    def test_unchanged_source_requires_matching_latest_and_source_sha_manifests(self):
        head = self.commit("docs/new.md")
        config = {"config": {"Labels": {"org.opencontainers.image.revision": self.base,
                                        "org.opencontainers.image.source": publisher.REPOSITORY}}}
        manifest = '{"schemaVersion":2,"config":{"digest":"sha256:fixture"}}'
        for immutable, rebuild in ((None, True), ("different manifest", True),
                                   ("", True), (manifest, False)):
            with self.subTest(immutable=immutable):
                real_run = publisher.run

                def command(*args, **kwargs):
                    if args[0] != "docker":
                        return real_run(*args, **kwargs)
                    if "--raw" not in args:
                        return subprocess.CompletedProcess(args, 0, json.dumps(config))
                    self.assertIn(args[4], ("image:latest", "image:" + self.base))
                    content = manifest if args[4] == "image:latest" else immutable
                    if content is None:
                        raise subprocess.CalledProcessError(1, args)
                    return subprocess.CompletedProcess(args, 0, content)

                with patch.object(publisher, "run", side_effect=command):
                    self.assertEqual(publisher.changed_since_publication("bot", head, "image", {}),
                                     rebuild)

    def test_partial_publication_is_repaired_before_fresh_build_skips(self):
        for component in ("bot", "api"):
            with self.subTest(component=component):
                image = "ghcr.io/jatmn/jmodguard-" + component
                registry = {}
                exports = []
                manifest = '{"schemaVersion":2,"config":{"digest":"sha256:fixture"}}'
                config = {"config": {"Labels": {"org.opencontainers.image.revision": self.base,
                                                "org.opencontainers.image.source": publisher.REPOSITORY}}}
                plan = {"mode": "main", "ref": "", "commit": self.base, "sha": self.base, "tag": ""}
                env = MAIN | {"BUILDKITE_COMMIT": self.base, "GHCR_USERNAME": "fixture",
                              "GHCR_TOKEN": "fixture-token", "GITHUB_TOKEN": "",
                              "BUILDKITE_GITHUB_EVENT": "push", "BUILDKITE_TAG": "",
                              "JMODGUARD_PUBLISH_SOURCE": ""}
                real_run = publisher.run

                def command(*args, **kwargs):
                    if args[0] == "git":
                        return real_run(*args, **kwargs)
                    if args[:3] == ("buildkite-agent", "meta-data", "get"):
                        return subprocess.CompletedProcess(args, 0, json.dumps(plan))
                    if args[:3] == ("docker", "buildx", "imagetools"):
                        if args[4] not in registry:
                            raise subprocess.CalledProcessError(1, args)
                        content = registry[args[4]] if "--raw" in args else json.dumps(config)
                        return subprocess.CompletedProcess(args, 0, content)
                    if args[:3] == ("docker", "buildx", "build"):
                        exports.append(args)
                        registry[image + ":latest"] = manifest
                        if len(exports) == 1:
                            raise subprocess.CalledProcessError(1, args)
                        registry[image + ":" + self.base] = manifest
                    return subprocess.CompletedProcess(args, 0, "")

                with patch.dict(os.environ, env), patch.object(publisher, "run", side_effect=command):
                    with self.assertRaises(subprocess.CalledProcessError):
                        publisher.publish(component)
                    self.assertNotIn(image + ":" + self.base, registry)
                    publisher.publish(component)
                    self.assertEqual(registry[image + ":" + self.base], registry[image + ":latest"])
                    self.assertEqual(len(exports), 2)
                    publisher.publish(component)
                    self.assertEqual(len(exports), 2)

    def test_retried_job_republishes_its_source_after_newer_build(self):
        newer = self.commit("docs/after.md")
        publisher.git("checkout", "--detach", self.base)
        for component in ("bot", "api"):
            with self.subTest(component=component):
                image = "ghcr.io/jatmn/jmodguard-" + component
                exports = []
                published = {image + ":latest", image + ":" + newer}
                config = {"config": {"Labels": {"org.opencontainers.image.revision": newer,
                                                "org.opencontainers.image.source": publisher.REPOSITORY}}}
                plan = {"mode": "main", "ref": "", "commit": self.base, "sha": self.base, "tag": ""}
                env = MAIN | {"BUILDKITE_COMMIT": self.base, "GHCR_USERNAME": "fixture",
                              "GHCR_TOKEN": "fixture-token", "GITHUB_TOKEN": "",
                              "BUILDKITE_GITHUB_EVENT": "push", "BUILDKITE_TAG": "",
                              "JMODGUARD_PUBLISH_SOURCE": "", "BUILDKITE_RETRY_COUNT": "1"}
                real_run = publisher.run

                def command(*args, **kwargs):
                    if args[0] == "git":
                        return real_run(*args, **kwargs)
                    if args[:3] == ("buildkite-agent", "meta-data", "get"):
                        return subprocess.CompletedProcess(args, 0, json.dumps(plan))
                    if args[:3] == ("docker", "buildx", "imagetools"):
                        if args[4] not in published:
                            raise subprocess.CalledProcessError(1, args)
                        content = '{"schemaVersion":2}' if "--raw" in args else json.dumps(config)
                        return subprocess.CompletedProcess(args, 0, content)
                    if args[:3] == ("docker", "buildx", "build"):
                        exports.append(args)
                        published.update(args[i + 1] for i, arg in enumerate(args) if arg == "--tag")
                    return subprocess.CompletedProcess(args, 0, "")

                with patch.dict(os.environ, env), patch.object(publisher, "run", side_effect=command):
                    publisher.publish(component)
                self.assertEqual(len(exports), 1)
                self.assertIn(image + ":" + self.base, published)


if __name__ == "__main__":
    unittest.main()
