#!/usr/bin/env python3
"""Publish one GHCR image from a trusted Buildkite build, without a GHA runner."""

import argparse
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile


REPOSITORY = "https://github.com/jatmn/jModGuard"
SHA = re.compile(r"[0-9a-f]{40}")
DOCKER_TAG = re.compile(r"[A-Za-z0-9_][A-Za-z0-9_.-]{0,127}")


def run(*args, **kwargs):
    kwargs.setdefault("check", True)
    kwargs["shell"] = False
    return subprocess.run(args, text=True, **kwargs)


def git(*args):
    return run("git", *args, capture_output=True).stdout.strip()


def request(env):
    """Classify before accessing registry credentials or running source code."""
    if env.get("BUILDKITE") != "true":
        raise ValueError("Image publication must run in Buildkite")
    if env.get("BUILDKITE_PULL_REQUEST", "false") != "false":
        return None
    source = env.get("BUILDKITE_SOURCE", "")
    branch = env.get("BUILDKITE_BRANCH", "")
    tag = env.get("BUILDKITE_TAG", "")
    manual = env.get("JMODGUARD_PUBLISH_SOURCE", "")
    # Honor the manual source only on UI/API builds. A leftover value on a
    # webhook push must not block automatic main/tag publication.
    if source in {"ui", "api"}:
        if not manual:
            return None
        if branch != "main" or tag:
            raise ValueError("Manual publication requires a UI/API build on main")
        # A full SHA or explicit tag avoids interpreting a branch as a source.
        if not SHA.fullmatch(manual) and not manual.startswith("refs/tags/"):
            raise ValueError("JMODGUARD_PUBLISH_SOURCE must be a full lowercase SHA or refs/tags/<tag>")
        return ("manual", manual)
    if tag and source == "webhook":
        return ("tag", "refs/tags/" + tag)
    if source == "webhook" and branch == "main" and not tag:
        # Older Buildkite integrations do not supply GITHUB_EVENT.
        if env.get("BUILDKITE_GITHUB_EVENT", "push") != "push":
            return None
        return ("main", "")
    return None


def resolve_source(mode, ref, head):
    if mode == "main":
        return head, ""
    if ref.startswith("refs/tags/"):
        run("git", "check-ref-format", ref, capture_output=True)
        # FETCH_HEAD binds this one fetch, including annotated tags, to one SHA.
        git("fetch", "--no-tags", "origin", ref)
        sha = git("rev-parse", "FETCH_HEAD^{commit}")
        label = ref[len("refs/tags/"):].replace("/", "-").replace("+", "-")
        if not DOCKER_TAG.fullmatch(label):
            print("Tag cannot be used as a Docker tag; publishing latest and SHA only")
            label = ""
        # Do not let a moving automatic tag substitute different source code.
        if mode == "tag" and sha != head:
            raise ValueError("Tag moved after the Buildkite checkout; create a new build")
    else:
        if not SHA.fullmatch(ref):
            raise ValueError("Expected an immutable full commit SHA")
        git("fetch", "--no-tags", "origin", ref)
        sha = git("rev-parse", "FETCH_HEAD^{commit}")
        if sha != ref:
            raise ValueError("Fetched commit differs from requested SHA")
        label = ""
    if not SHA.fullmatch(sha):
        raise ValueError("Source did not resolve to a full commit SHA")
    return sha, label


def changed_since_publication(component, sha, image, env):
    """Compare to published source, not HEAD^ (which can miss multi-commit pushes)."""
    try:
        config = json.loads(run(
            "docker", "buildx", "imagetools", "inspect", image + ":latest",
            "--format", "{{json .Image}}", capture_output=True, env=env,
        ).stdout)
        labels = config["config"]["Labels"]
        previous = labels["org.opencontainers.image.revision"]
        if labels.get("org.opencontainers.image.source") != REPOSITORY or not SHA.fullmatch(previous):
            return True
        # Both Dockerfiles copy these shared source trees. Include publishing
        # configuration so migration/recipe changes cannot be skipped.
        paths = ["Dockerfile." + component, "Cargo.toml", "Cargo.lock",
                 "rust-toolchain.toml", "src", "crates", "migrations", "vendor", ".buildkite"]
        if git("diff", "--name-only", previous, sha, "--", *paths):
            return True
        # Tag pushes are separate writes: latest can succeed before the SHA
        # push fails. A retry must repair that partial publication, not skip it.
        latest = run("docker", "buildx", "imagetools", "inspect", image + ":latest",
                     "--raw", capture_output=True, env=env).stdout
        immutable = run("docker", "buildx", "imagetools", "inspect", image + ":" + previous,
                        "--raw", capture_output=True, env=env).stdout
        return not latest.strip() or latest != immutable
    except (subprocess.CalledProcessError, ValueError, KeyError, TypeError):
        # First publish, absent history, registry failures and foreign manifests
        # all require a build. They must never look like unchanged source.
        return True


def build_args(component, sha, label, context):
    image = "ghcr.io/jatmn/jmodguard-" + component
    args = ["docker", "buildx", "build", "--builder", "default",
            "--file", str(Path(context) / ("Dockerfile." + component)),
            "--platform", "linux/amd64", "--provenance=false", "--sbom=false",
            "--push", "--progress", "plain",
            "--cache-from", "type=registry,ref=" + image + ":latest",
            "--cache-to", "type=inline",
            "--secret", "id=github_token,env=GITHUB_TOKEN",
            "--label", "org.opencontainers.image.source=" + REPOSITORY,
            "--label", "org.opencontainers.image.revision=" + sha,
            "--label", "org.opencontainers.image.version=" + (label or sha),
            "--tag", image + ":latest", "--tag", image + ":" + sha]
    if label and label not in {"latest", sha}:
        args += ["--tag", image + ":" + label]
    return args + [str(context)]


def checked_request(env):
    selection = request(env)
    if selection is None:
        return None
    head = git("rev-parse", "HEAD")
    if not SHA.fullmatch(head) or env.get("BUILDKITE_COMMIT") != head:
        raise ValueError("Buildkite commit does not match the checked-out full SHA")
    if git("status", "--porcelain", "--untracked-files=no"):
        raise ValueError("Refusing to publish a checkout with tracked modifications")
    return (*selection, head)


def prepare():
    selection = checked_request(os.environ)
    if selection is None:
        print("Test-only build; no image publication")
        return
    mode, ref, head = selection
    exists = run("buildkite-agent", "meta-data", "exists", "image-publication",
                 check=False, capture_output=True)
    if exists.returncode == 0:
        read_plan(selection)
        print("Reusing the original immutable publication plan")
        return
    if exists.returncode != 100:
        raise ValueError("Could not check existing publication plan")
    sha, label = resolve_source(mode, ref, head)
    # Resolve moving tags once for both components, even on different agents.
    plan = {"mode": mode, "ref": ref, "commit": head, "sha": sha, "tag": label}
    run("buildkite-agent", "meta-data", "set", "image-publication", json.dumps(plan))


def read_plan(selection):
    mode, ref, head = selection
    planned = json.loads(run("buildkite-agent", "meta-data", "get", "image-publication",
                             capture_output=True).stdout)
    if not isinstance(planned, dict) or not all(isinstance(planned.get(k), str)
                                               for k in ("mode", "ref", "commit", "sha", "tag")):
        raise ValueError("Invalid image publication plan")
    sha, label = planned["sha"], planned["tag"]
    if (planned["mode"] != mode or planned["ref"] != ref or planned["commit"] != head
            or not SHA.fullmatch(sha) or (label and not DOCKER_TAG.fullmatch(label))):
        raise ValueError("Invalid image publication plan")
    if mode != "manual" and sha != head:
        raise ValueError("Publication plan differs from the Buildkite commit")
    return sha, label


def publish(component):
    env = os.environ.copy()
    selection = checked_request(env)
    if selection is None:
        print("Test-only build; no image publication")
        return
    mode, _, head = selection
    sha, label = read_plan(selection)
    if mode == "manual":
        # The prepare job may run on a different agent with a different clone.
        resolve_source("manual", sha, head)
    image = "ghcr.io/jatmn/jmodguard-" + component
    # An explicit retry must restore its selected source's tags even if a
    # newer build has since published identical inputs under a different SHA.
    # Inspect public GHCR history before fetching push credentials so docs-only
    # main pushes skip without GHCR_TOKEN or docker login.
    if (mode == "main" and env.get("BUILDKITE_RETRY_COUNT", "0") == "0"
            and not changed_since_publication(component, sha, image, env)):
        print(f"{component}: image inputs match the last publication; no rebuild")
        return
    username = env.get("GHCR_USERNAME", "")
    if not username:
        raise ValueError("Set GHCR_USERNAME to the GitHub account owning the GHCR credential")
    token = env.get("GHCR_TOKEN") or run(
        "buildkite-agent", "secret", "get", "GHCR_TOKEN", capture_output=True,
    ).stdout.strip()
    if not token:
        raise ValueError("Buildkite secret GHCR_TOKEN is empty")
    # Match the CI jobs' handling of an old literal Buildkite token mapping.
    source_token = env.get("GITHUB_TOKEN")
    if source_token in (None, "", "$GITHUB_TOKEN"):
        source_token = token
    # Register inherited credentials too; never send tokens in command arguments.
    run("buildkite-agent", "redactor", "add", "--format", "json",
        input=json.dumps({"GHCR_TOKEN": token, "GITHUB_TOKEN": source_token}), capture_output=True)
    env["GITHUB_TOKEN"] = source_token
    scratch_root = Path(env.get("TMPDIR") or ".tmp").resolve()
    scratch_root.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="publish-", dir=scratch_root) as scratch:
        env["DOCKER_CONFIG"] = str(Path(scratch) / "docker")
        Path(env["DOCKER_CONFIG"]).mkdir(mode=0o700)
        run("docker", "login", "ghcr.io", "--username", username,
            "--password-stdin", input=token, env=env, capture_output=True)
        # A separate clean worktree excludes CI output and any untracked source.
        # Manual builds use current main's publisher to build older source safely.
        context = Path(scratch) / "source"
        run("git", "worktree", "add", "--detach", str(context), sha, capture_output=True)
        try:
            run(*build_args(component, sha, label, context), env=env)
        finally:
            run("git", "worktree", "remove", "--force", str(context), capture_output=True)
        print(f"Published {image}:{sha}")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("component", choices=("prepare", "bot", "api"))
    args = parser.parse_args()
    try:
        if args.component == "prepare":
            prepare()
        else:
            publish(args.component)
    except (ValueError, subprocess.CalledProcessError) as error:
        # CalledProcessError's repr does not include captured secret output.
        parser.exit(1, f"Image publication failed: {error}\n")
