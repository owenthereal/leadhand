#!/usr/bin/env python3
"""The Codex worker's environment (spec contract item 9), against real Git and `gh`.

Converted from tests/test_worker_env.py. Each scenario runs one `dispatch` with a parent
environment the test chooses (credentials, Git configuration, SSH and askpass programs), and
takes the environment the worker really got from the fake Codex's log (FAKE_CODEX_LOG). Git
and `gh` then run with exactly that environment, so what they do is the evidence: a refused push
leaves a local bare repository without the ref, and a program the parent configured leaves no
marker file. No network: remote hosts are `.invalid`, and the HTTP remote is a local server
answering 401.
"""
import http.server
import os
import shutil
import subprocess
import threading
import time
import unittest
from pathlib import Path

from contract_dispatch import World
from gitfixture import make_repo, run_git

# What a parent may hold that the worker must not see.
TOKENS = ("GH_TOKEN", "GITHUB_TOKEN", "GH_ENTERPRISE_TOKEN", "GITHUB_ENTERPRISE_TOKEN",
          "SSH_AUTH_SOCK", "GIT_ASKPASS", "SSH_ASKPASS")


class Answer401(http.server.BaseHTTPRequestHandler):
    def _deny(self):
        self.send_response(401)
        self.send_header("WWW-Authenticate", 'Basic realm="leadhand test"')
        self.send_header("Content-Length", "0")
        self.end_headers()

    do_GET = do_POST = _deny

    def log_message(self, *args):
        pass


class WorkerEnvironment(unittest.TestCase):
    def worker_env(self, parent=lambda tmp: {}) -> tuple[World, dict, dict]:
        """Dispatch once with every credential in TOKENS and `parent(tmp)` (variables, made with
        files under the world's temp dir) in the parent environment. Returns the world, the
        environment the fake Codex was started with, and the `shell_environment_policy.set`
        entries on its command line. `self.repo` is a fresh repository for Git to act in."""
        w = World(self)
        w.scenario("ready")
        w.env.update({name: f"parent-{name.lower()}" for name in TOKENS})
        w.env.update(parent(w.tmp))
        w.dispatch().ok()
        [start] = w.records("start")
        self.repo = make_repo(w.tmp, name="worker repo")
        return w, start["env"], start["policy_set"]

    def git(self, env, *args, timeout=10):
        return subprocess.run(["git", "-c", "user.name=W", "-c", "user.email=w@example.invalid",
                               "-c", "commit.gpgsign=false", *args],
                              cwd=self.repo, env=env, capture_output=True, text=True,
                              stdin=subprocess.DEVNULL, timeout=timeout)

    def local_401_server(self) -> str:
        server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Answer401)
        threading.Thread(target=server.serve_forever, daemon=True).start()
        self.addCleanup(server.server_close)
        self.addCleanup(server.shutdown)
        return f"http://127.0.0.1:{server.server_port}/owner/repo.git"

    def test_worker_env_removes_tokens_and_sets_empty_gh_config(self):
        def parent(tmp):
            gh = tmp / "parent gh config"
            gh.mkdir()
            (gh / "hosts.yml").write_text("github.com:\n    oauth_token: parent-hosts-token\n", encoding="utf-8")
            return {"GH_CONFIG_DIR": str(gh)}
        w, env, policy = self.worker_env(parent)
        parent_gh = w.tmp / "parent gh config"
        for name in TOKENS:
            self.assertFalse(name in env, f"{name} reached the worker env")
            self.assertFalse(name in policy, f"{name} reached the shell policy")
        self.assertEqual(env["LEADHAND_RUN_ID"], w.run_id)
        gh_dir = Path(env["GH_CONFIG_DIR"])
        self.assertTrue(gh_dir.is_dir())
        self.assertNotEqual(gh_dir.resolve(), parent_gh.resolve())
        self.assertEqual(os.listdir(gh_dir), [])
        self.assertEqual(policy["GH_CONFIG_DIR"], env["GH_CONFIG_DIR"])
        self.assertEqual(env["GIT_SSH_COMMAND"], "false")
        self.assertEqual(env["GIT_TERMINAL_PROMPT"], "0")
        self.assertEqual(env["PATH"], w.env["PATH"])
        r = self.git(env, "config", "--get-all", "credential.helper")
        self.assertEqual(r.stdout.splitlines()[-1:], [""])

    def test_existing_git_config_env_entries_preserved(self):
        _, env, policy = self.worker_env(lambda tmp: {
            "GIT_CONFIG_COUNT": "2", "GIT_CONFIG_KEY_0": "user.name", "GIT_CONFIG_VALUE_0": "Kept Name",
            "GIT_CONFIG_KEY_1": "core.abbrev", "GIT_CONFIG_VALUE_1": "12"})
        self.assertEqual(env["GIT_CONFIG_COUNT"], "3")
        self.assertEqual((env["GIT_CONFIG_KEY_2"], env["GIT_CONFIG_VALUE_2"]), ("credential.helper", ""))
        for name in ("GIT_CONFIG_COUNT", *(f"GIT_CONFIG_{kv}_{i}" for kv in ("KEY", "VALUE") for i in range(3))):
            self.assertEqual(policy[name], env[name], name)
        self.assertEqual(subprocess.run(["git", "config", "user.name"], cwd=self.repo, env=env,
                                        capture_output=True, text=True, timeout=10).stdout.strip(), "Kept Name")
        self.assertEqual(subprocess.run(["git", "config", "core.abbrev"], cwd=self.repo, env=env,
                                        capture_output=True, text=True, timeout=10).stdout.strip(), "12")

    def test_existing_git_config_parameters_kept_and_askpass_cleared(self):
        _, env, policy = self.worker_env(lambda tmp: {"GIT_CONFIG_PARAMETERS": "'user.email=kept@example.invalid'"})
        self.assertEqual(policy["GIT_CONFIG_PARAMETERS"], env["GIT_CONFIG_PARAMETERS"])
        self.assertEqual(subprocess.run(["git", "config", "user.email"], cwd=self.repo, env=env,
                                        capture_output=True, text=True, timeout=10).stdout.strip(),
                         "kept@example.invalid")
        r = subprocess.run(["git", "config", "--get-all", "core.askPass"], cwd=self.repo, env=env,
                           capture_output=True, text=True, timeout=10)
        self.assertEqual(r.stdout.splitlines()[-1:], [""])

    def test_ssh_push_fails_for_ssh_scp_and_explicit_pushurl(self):
        def parent(tmp):
            fake_ssh = tmp / "fake ssh.sh"
            fake_ssh.write_text(f"#!/bin/sh\ntouch '{tmp / 'ssh was called'}'\nexit 0\n")
            fake_ssh.chmod(0o755)
            return {"GIT_SSH_COMMAND": f"'{fake_ssh}'", "GIT_SSH": str(fake_ssh)}
        w, env, _ = self.worker_env(parent)
        tmp = w.tmp
        marker = tmp / "ssh was called"
        run_git(self.repo, "remote", "add", "sshurl", "ssh://git@example.invalid/owner/repo.git")
        run_git(self.repo, "remote", "add", "scp", "git@example.invalid:owner/repo.git")
        bare = tmp / "local bare.git"
        run_git(tmp, "init", "--quiet", "--bare", str(bare))
        run_git(self.repo, "remote", "add", "pushurl", str(bare))
        run_git(self.repo, "remote", "set-url", "--push", "pushurl", "git@example.invalid:owner/repo.git")
        for remote in ("sshurl", "scp", "pushurl"):
            t0 = time.monotonic()
            r = self.git(env, "push", remote, "HEAD:refs/heads/x")
            self.assertNotEqual(r.returncode, 0, remote)
            self.assertLess(time.monotonic() - t0, 10, remote)
        self.assertFalse(marker.exists(), "the parent's ssh command ran")
        self.assertEqual(run_git(bare, "for-each-ref"), "")

    def test_https_push_without_credentials_fails_without_prompting(self):
        url = self.local_401_server()

        def parent(tmp):
            helper = tmp / "cred helper.sh"
            helper.write_text(f"#!/bin/sh\ntouch '{tmp / 'helper was called'}'\necho username=u\necho password=p\n")
            helper.chmod(0o755)
            # The parent's Git config names a credential helper; the worker env must reset it.
            return {"GIT_CONFIG_COUNT": "1", "GIT_CONFIG_KEY_0": "credential.helper",
                    "GIT_CONFIG_VALUE_0": f"!'{helper}'", "NO_PROXY": "127.0.0.1", "no_proxy": "127.0.0.1"}
        w, env, _ = self.worker_env(parent)
        helper_marker = w.tmp / "helper was called"
        t0 = time.monotonic()
        r = self.git(env, "push", url, "HEAD:refs/heads/x", timeout=10)
        self.assertLess(time.monotonic() - t0, 10)
        self.assertNotEqual(r.returncode, 0)
        self.assertFalse(helper_marker.exists(), "the parent's credential helper ran")
        self.assertRegex(r.stderr, r"terminal prompts disabled|Authentication failed|could not read Username")

    def test_configured_askpass_is_not_run(self):
        url = self.local_401_server()

        def parent(tmp):
            askpass = tmp / "ask pass.sh"
            askpass.write_text(f"#!/bin/sh\ntouch '{tmp / 'askpass was called'}'\necho secret\n")
            askpass.chmod(0o755)
            # The user's Git config names an askpass program (here through the parent's GIT_CONFIG_*).
            return {"GIT_CONFIG_COUNT": "1", "GIT_CONFIG_KEY_0": "core.askPass", "GIT_CONFIG_VALUE_0": str(askpass),
                    "NO_PROXY": "127.0.0.1", "no_proxy": "127.0.0.1"}
        w, env, _ = self.worker_env(parent)
        marker = w.tmp / "askpass was called"
        r = self.git(env, "push", url, "HEAD:refs/heads/x", timeout=10)
        self.assertNotEqual(r.returncode, 0)
        self.assertFalse(marker.exists(), "the configured askpass program ran")

    @unittest.skipUnless(shutil.which("gh"), "gh is not installed")
    def test_gh_reports_no_login_with_parent_token(self):
        _, env, _ = self.worker_env(lambda tmp: {"GH_TOKEN": "ghp_parenttokenvalue0000000000000000000000"})
        r = subprocess.run(["gh", "auth", "status"], env=env, capture_output=True, text=True,
                           stdin=subprocess.DEVNULL, timeout=30)
        self.assertNotEqual(r.returncode, 0, r.stdout + r.stderr)
        self.assertIn("not logged into any GitHub hosts", r.stdout + r.stderr)
        self.assertNotIn("GH_TOKEN", r.stdout + r.stderr)

    def test_local_path_push_still_allowed(self):
        # The documented limit: a push to a local path needs no credentials, so it works.
        w, env, _ = self.worker_env()
        bare = w.tmp / "local bare.git"
        run_git(w.tmp, "init", "--quiet", "--bare", str(bare))
        r = self.git(env, "push", str(bare), "HEAD:refs/heads/x")
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(run_git(bare, "rev-parse", "refs/heads/x"), run_git(self.repo, "rev-parse", "HEAD"))


if __name__ == "__main__":
    unittest.main()
