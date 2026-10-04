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

# A replaced binary may contain the old installer. SIGKILL cannot run cleanup,
# so its next update must recover the PID published before the atomic rename.
case = root / "sync-sigkill-legacy-recovery"
home = case / "home"
shutil.copytree(original_home, home)
ready, release = case / "ready", case / "release"
env = os.environ.copy()
env.update(HOME=str(home), USAGI_HOME=str(home / ".usagi"),
           FIXTURE_DIR=fixture, PATH=f"{fake_bin}:{env['PATH']}",
           USAGI_MANAGED_UPDATE="1", USAGI_VERSION="v2.0.0",
           USAGI_SYNC_LOG=str(ready), USAGI_SYNC_WAIT_FOR=str(release))
child = subprocess.Popen(["bash", installer], cwd=cwd, env=env,
                         stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                         text=True, start_new_session=True)
legacy = None
try:
    deadline = time.monotonic() + 15
    while not ready.exists():
        assert child.poll() is None, child.communicate()
        assert time.monotonic() < deadline, "installer did not reach sync"
        time.sleep(0.005)
    data = home / ".usagi"
    assert (data / "update.lock/pid").read_text().strip() == str(child.pid)
    child.kill()
    release.touch()
    child.communicate(timeout=15)
    assert child.returncode == -signal.SIGKILL
    assert subprocess.check_output([str(data / "bin/usagi"), "--version"],
                                   text=True).strip() == "usagi 2.0.0"
    legacy = subprocess.Popen([
        "/bin/bash", str(Path(__file__).parent / "fixtures/install-legacy-lock.sh"),
        str(data), str(case),
    ], stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
        start_new_session=True)
    deadline = time.monotonic() + 15
    while not (case / "acquired-legacy").exists():
        assert legacy.poll() is None, legacy.communicate()
        assert time.monotonic() < deadline, "older installer could not recover crashed update"
        time.sleep(0.005)
    (case / "release-legacy").touch()
    output, error = legacy.communicate(timeout=15)
    assert legacy.returncode == 0, (output, error)
    assert not (data / "update.lock").exists()
    env.pop("USAGI_SYNC_LOG")
    env.pop("USAGI_SYNC_WAIT_FOR")
    retry = subprocess.run(["bash", installer], cwd=cwd, env=env,
                           capture_output=True, text=True, timeout=15)
    assert retry.returncode == 0, (retry.stdout, retry.stderr)
    print("installer_sigkill_after_replacement_allows_legacy_recovery_and_retry: passed")
finally:
    release.touch()
    (case / "release-legacy").touch()
    for process in (child, legacy):
        if process is None:
            continue
        if process.poll() is None:
            os.killpg(process.pid, signal.SIGKILL)
        try:
            process.communicate(timeout=15)
        except subprocess.TimeoutExpired:
            os.killpg(process.pid, signal.SIGKILL)
            process.communicate(timeout=15)
