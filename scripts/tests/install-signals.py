#!/usr/bin/env python3
"""Interrupt the real installer before/after commit, then retry the update."""

from pathlib import Path
import os
import shutil
import signal
import subprocess
import sys
import time


installer, original_home, fixture, fake_bin, cwd = sys.argv[1:]
root = Path(original_home).parent

for phase in ("download", "sync"):
    for interrupted in (signal.SIGHUP, signal.SIGINT, signal.SIGTERM):
        case = root / f"{phase}-{interrupted.name}"
        home = case / "home"
        shutil.copytree(original_home, home)
        ready, release = case / "ready", case / "release"
        env = os.environ.copy()
        env.update(HOME=str(home), USAGI_HOME=str(home / ".usagi"),
                   FIXTURE_DIR=fixture, PATH=f"{fake_bin}:{env['PATH']}",
                   USAGI_MANAGED_UPDATE="1", USAGI_VERSION="v2.0.0")
        if phase == "download":
            env.update(FAKE_CURL_READY=str(ready), FAKE_CURL_WAIT_FOR=str(release))
        else:
            env.update(USAGI_SYNC_LOG=str(ready), USAGI_SYNC_WAIT_FOR=str(release))
        child = subprocess.Popen(["bash", installer], cwd=cwd, env=env,
                                 stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                 text=True, start_new_session=True)
        try:
            deadline = time.monotonic() + 15
            while not ready.exists():
                assert child.poll() is None, child.communicate()
                assert time.monotonic() < deadline, f"{case}: missing ready marker"
                time.sleep(0.005)
            child.send_signal(interrupted)
            # Bash handles a trapped signal after its foreground command exits.
            # Release that command rather than leaving the fixture blocked.
            release.touch()
            output, error = child.communicate(timeout=15)
            assert child.returncode == 128 + interrupted, (child.returncode, output, error)
            assert "次回の起動から新しい CLI" not in output, output
            data = home / ".usagi"
            assert not list((data / "bin").glob(".update.*"))
            assert not list((data / "update.lock").glob("owner.*"))
            installed = subprocess.check_output([str(data / "bin/usagi"), "--version"],
                                                text=True).strip()
            assert installed == ("usagi 1.0.0" if phase == "download" else "usagi 2.0.0")
            for name in ("FAKE_CURL_READY", "FAKE_CURL_WAIT_FOR",
                         "USAGI_SYNC_LOG", "USAGI_SYNC_WAIT_FOR"):
                env.pop(name, None)
            retry = subprocess.run(["bash", installer], cwd=cwd, env=env,
                                   capture_output=True, text=True, timeout=15)
            assert retry.returncode == 0, (retry.stdout, retry.stderr)
            assert "daemon の build を同期した" in retry.stdout
            assert subprocess.check_output([str(data / "bin/usagi"), "--version"],
                                           text=True).strip() == "usagi 2.0.0"
            print(f"installer_{phase}_{interrupted.name.lower()}_cleanup_and_retry: passed")
        finally:
            release.touch()
            if child.poll() is None:
                os.killpg(child.pid, signal.SIGKILL)
            child.communicate(timeout=15)
