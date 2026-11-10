"""The worker environment against real Git (and `gh` when installed). No network:
remote hosts are `.invalid`, and the HTTPS remote is a local server answering 401."""
import http.server
import os
import shutil
import subprocess
import tempfile
import threading
import time
import unittest
from pathlib import Path

from gitfixture import HERMETIC_ENV, LEAKY_VARS, hermetic, make_repo, run_git
from leadhand.dispatch import worker_env

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


class WorkerEnvTests(unittest.TestCase):
    def setUp(self):
        hermetic(self)
        self.tmp = Path(tempfile.mkdtemp(prefix="lh worker env "))
        self.addCleanup(shutil.rmtree, self.tmp, True)
        self.repo = make_repo(self.tmp)
        self.gh_dir = self.tmp / "gh config"
        self.gh_dir.mkdir()

    def parent(self, **extra):
        env = {k: v for k, v in os.environ.items() if k not in LEAKY_VARS}
        env.update(HERMETIC_ENV)
        env.update({name: f"parent-{name.lower()}" for name in TOKENS})
        env.update(extra)
        return env

    def env(self, **extra):
        return worker_env(self.parent(**extra), run_id="run-123", gh_config_dir=self.gh_dir)

    def git(self, env, *args, timeout=10):
        return subprocess.run(["git", "-c", "user.name=W", "-c", "user.email=w@example.invalid",
                               "-c", "commit.gpgsign=false", *args],
                              cwd=self.repo, env=env, capture_output=True, text=True,
                              stdin=subprocess.DEVNULL, timeout=timeout)

    def test_worker_env_removes_tokens_and_sets_empty_gh_config(self):
        env = self.env()
        for name in TOKENS:
            self.assertFalse(name in env, f"{name} reached the worker env")
        self.assertEqual(env["LEADHAND_RUN_ID"], "run-123")
        self.assertEqual(env["GH_CONFIG_DIR"], str(self.gh_dir))
        self.assertEqual(os.listdir(self.gh_dir), [])
        self.assertEqual(env["GIT_SSH_COMMAND"], "false")
        self.assertEqual(env["GIT_TERMINAL_PROMPT"], "0")
        self.assertEqual(env["PATH"], os.environ["PATH"])
        r = self.git(env, "config", "--get-all", "credential.helper")
        self.assertEqual(r.stdout.splitlines()[-1:], [""])

    def test_existing_git_config_env_entries_preserved(self):
        env = self.env(GIT_CONFIG_COUNT="2", GIT_CONFIG_KEY_0="user.name", GIT_CONFIG_VALUE_0="Kept Name",
                       GIT_CONFIG_KEY_1="core.abbrev", GIT_CONFIG_VALUE_1="12")
        self.assertEqual(env["GIT_CONFIG_COUNT"], "3")
        self.assertEqual((env["GIT_CONFIG_KEY_2"], env["GIT_CONFIG_VALUE_2"]), ("credential.helper", ""))
        self.assertEqual(subprocess.run(["git", "config", "user.name"], cwd=self.repo, env=env,
                                        capture_output=True, text=True, timeout=10).stdout.strip(), "Kept Name")
        self.assertEqual(subprocess.run(["git", "config", "core.abbrev"], cwd=self.repo, env=env,
                                        capture_output=True, text=True, timeout=10).stdout.strip(), "12")

    def test_ssh_push_fails_for_ssh_scp_and_explicit_pushurl(self):
        marker = self.tmp / "ssh was called"
        fake_ssh = self.tmp / "fake ssh.sh"
        fake_ssh.write_text(f"#!/bin/sh\ntouch '{marker}'\nexit 0\n")
        fake_ssh.chmod(0o755)
        env = self.env(GIT_SSH_COMMAND=f"'{fake_ssh}'", GIT_SSH=str(fake_ssh))
        run_git(self.repo, "remote", "add", "sshurl", "ssh://git@example.invalid/owner/repo.git")
        run_git(self.repo, "remote", "add", "scp", "git@example.invalid:owner/repo.git")
        bare = self.tmp / "local bare.git"
        run_git(self.tmp, "init", "--quiet", "--bare", str(bare))
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
        server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Answer401)
        threading.Thread(target=server.serve_forever, daemon=True).start()
        self.addCleanup(server.server_close)
        self.addCleanup(server.shutdown)
        url = f"http://127.0.0.1:{server.server_port}/owner/repo.git"
        helper_marker = self.tmp / "helper was called"
        helper = self.tmp / "cred helper.sh"
        helper.write_text(f"#!/bin/sh\ntouch '{helper_marker}'\necho username=u\necho password=p\n")
        helper.chmod(0o755)
        # The parent's Git config names a credential helper; the worker env must reset it.
        env = self.env(GIT_CONFIG_COUNT="1", GIT_CONFIG_KEY_0="credential.helper",
                       GIT_CONFIG_VALUE_0=f"!'{helper}'", NO_PROXY="127.0.0.1", no_proxy="127.0.0.1")
        t0 = time.monotonic()
        r = self.git(env, "push", url, "HEAD:refs/heads/x", timeout=10)
        self.assertLess(time.monotonic() - t0, 10)
        self.assertNotEqual(r.returncode, 0)
        self.assertFalse(helper_marker.exists(), "the parent's credential helper ran")
        self.assertRegex(r.stderr, r"terminal prompts disabled|Authentication failed|could not read Username")

    @unittest.skipUnless(shutil.which("gh"), "gh is not installed")
    def test_gh_reports_no_login_with_parent_token(self):
        env = self.env(GH_TOKEN="ghp_parenttokenvalue0000000000000000000000")
        r = subprocess.run(["gh", "auth", "status"], env=env, capture_output=True, text=True,
                           stdin=subprocess.DEVNULL, timeout=30)
        self.assertNotEqual(r.returncode, 0, r.stdout + r.stderr)
        self.assertIn("not logged into any GitHub hosts", r.stdout + r.stderr)
        self.assertNotIn("GH_TOKEN", r.stdout + r.stderr)

    def test_local_path_push_still_allowed(self):
        # The documented limit: a push to a local path needs no credentials, so it works.
        bare = self.tmp / "local bare.git"
        run_git(self.tmp, "init", "--quiet", "--bare", str(bare))
        r = self.git(self.env(), "push", str(bare), "HEAD:refs/heads/x")
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertTrue(run_git(bare, "rev-parse", "refs/heads/x"))


if __name__ == "__main__":
    unittest.main()
