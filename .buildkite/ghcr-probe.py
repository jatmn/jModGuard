#!/usr/bin/env python3
"""Prove GHCR credentials from a Buildkite job without touching latest/SHA tags."""

import json
import os
from pathlib import Path
import subprocess
import tempfile
import urllib.error
import urllib.request

PINNED = "f2391d613c4bb8018873964e779341f50ce05447"
TAG = "bk-cred-audit-20260908"
IMAGES = ("ghcr.io/jatmn/jmodguard-bot", "ghcr.io/jatmn/jmodguard-api")
RESULT = Path("/tmp/jmodguard-ghcr-probe.json")


def run(*args, **kwargs):
    kwargs.setdefault("check", True)
    return subprocess.run(args, text=True, **kwargs)


def record(data):
    RESULT.write_text(json.dumps(data, indent=2) + "\n")
    print(json.dumps(data, indent=2))


def github(token, path):
    req = urllib.request.Request(
        "https://api.github.com" + path,
        headers={
            "Authorization": "Bearer " + token,
            "Accept": "application/vnd.github+json",
            "User-Agent": "jmodguard-ghcr-probe",
            "X-GitHub-Api-Version": "2022-11-28",
        },
    )
    try:
        with urllib.request.urlopen(req, timeout=30) as resp:
            body = resp.read()
            return resp.status, dict(resp.headers), json.loads(body) if body else {}
    except urllib.error.HTTPError as error:
        return error.code, dict(error.headers), {}


def main():
    out = {"ok": False, "steps": {}}
    if os.environ.get("BUILDKITE") != "true":
        out["steps"]["buildkite"] = "not a Buildkite job"
        record(out)
        raise SystemExit(1)

    username = os.environ.get("GHCR_USERNAME", "")
    out["steps"]["ghcr_username"] = {"set": bool(username), "value": username or None}
    if not username:
        record(out)
        raise SystemExit(1)

    token = os.environ.get("GHCR_TOKEN") or ""
    source = "env"
    if not token:
        got = run("buildkite-agent", "secret", "get", "GHCR_TOKEN", capture_output=True, check=False)
        token = (got.stdout or "").strip()
        source = "secret get"
        if got.returncode != 0:
            err = (got.stderr or got.stdout or "").strip()
            out["steps"]["ghcr_token"] = {"present": False, "source": source, "error": err[:300]}
            record(out)
            raise SystemExit(1)
    out["steps"]["ghcr_token"] = {
        "present": True,
        "source": source,
        "kind": "classic" if token.startswith("ghp_") else
        "fine-grained" if token.startswith("github_pat_") else
        "oauth" if token.startswith("gho_") else
        "server" if token.startswith("ghs_") else "other",
        "nonempty": bool(token),
    }
    if not token:
        record(out)
        raise SystemExit(1)

    run("buildkite-agent", "redactor", "add", "--format", "json",
        input=json.dumps({"GHCR_TOKEN": token}), capture_output=True)

    status, headers, user = github(token, "/user")
    scopes = headers.get("X-OAuth-Scopes") or headers.get("x-oauth-scopes") or ""
    out["steps"]["github_auth"] = {
        "http": status,
        "login": user.get("login"),
        "oauth_scopes": scopes,
        "write_packages": "write:packages" in scopes,
        "repo": "repo" in scopes.split(", ") if scopes else None,
    }

    packages = {}
    for name in ("jmodguard-bot", "jmodguard-api"):
        http, _, body = github(token, "/users/jatmn/packages/container/" + name)
        packages[name] = {"http": http, "visibility": body.get("visibility"), "name": body.get("name")}
    out["steps"]["packages"] = packages

    scratch_root = Path(os.environ.get("TMPDIR") or ".tmp").resolve()
    scratch_root.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="ghcr-probe-", dir=scratch_root) as scratch:
        scratch = Path(scratch)
        token_file = scratch / "token"
        token_file.write_text(token)
        token_file.chmod(0o600)
        probe_env = os.environ.copy()
        probe_env["GIT_CONFIG_COUNT"] = "1"
        probe_env["GIT_CONFIG_KEY_0"] = "credential.helper"
        probe_env["GIT_CONFIG_VALUE_0"] = (
            "!f() { echo username=x-access-token; echo password=$(cat %s); }; f" % token_file
        )
        git = run(
            "git", "ls-remote", "https://github.com/jatmn/jModGuard.git", PINNED,
            capture_output=True, env=probe_env, check=False,
        )
        fetched = run(
            "git", "init", str(scratch / "src"), capture_output=True, env=probe_env, check=False,
        )
        pin_ok = False
        if fetched.returncode == 0:
            run("git", "-C", str(scratch / "src"), "remote", "add", "origin",
                "https://github.com/jatmn/jModGuard.git", capture_output=True, env=probe_env)
            pin = run(
                "git", "-C", str(scratch / "src"), "fetch", "--no-tags", "--depth=1",
                "origin", PINNED, capture_output=True, env=probe_env, check=False,
            )
            pin_ok = pin.returncode == 0
        out["steps"]["source_read"] = {
            "ls_remote_ok": git.returncode == 0,
            "pinned_fetch_ok": pin_ok,
            "pinned": PINNED,
        }

        docker_env = os.environ.copy()
        docker_env["DOCKER_CONFIG"] = str(scratch / "docker")
        Path(docker_env["DOCKER_CONFIG"]).mkdir(mode=0o700)
        login = run(
            "docker", "login", "ghcr.io", "--username", username, "--password-stdin",
            input=token, env=docker_env, capture_output=True, check=False,
        )
        out["steps"]["docker_login"] = {"ok": login.returncode == 0}

        inspect = {}
        pushed = {}
        if login.returncode == 0:
            for image in IMAGES:
                look = run(
                    "docker", "buildx", "imagetools", "inspect", image + ":latest",
                    capture_output=True, env=docker_env, check=False,
                )
                inspect[image] = {"latest_readable": look.returncode == 0}
                if look.returncode != 0:
                    pushed[image] = {"ok": False, "reason": "latest not readable"}
                    continue
                create = run(
                    "docker", "buildx", "imagetools", "create",
                    "--tag", image + ":" + TAG, image + ":latest",
                    capture_output=True, env=docker_env, check=False,
                )
                pushed[image] = {
                    "ok": create.returncode == 0,
                    "tag": TAG,
                    "overwrote_latest": False,
                }
        out["steps"]["inspect"] = inspect
        out["steps"]["temporary_tag_push"] = pushed

    out["ok"] = bool(
        out["steps"]["ghcr_username"]["set"]
        and out["steps"]["ghcr_token"]["present"]
        and out["steps"].get("github_auth", {}).get("http") == 200
        and out["steps"].get("source_read", {}).get("pinned_fetch_ok")
        and out["steps"].get("docker_login", {}).get("ok")
        and all(item.get("ok") for item in out["steps"].get("temporary_tag_push", {}).values())
    )
    record(out)
    raise SystemExit(0 if out["ok"] else 1)


if __name__ == "__main__":
    main()
