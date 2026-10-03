#!/usr/bin/env python3
"""Deterministic process interleavings for the installer's lock protocol."""

from pathlib import Path
import os
import re
import signal
import subprocess
import sys
import tempfile
import time


def definition(source, name):
    match = re.search(r"^" + name + r"\(\) \{\n.*?^\}", source, re.M | re.S)
    assert match, name
    return match.group()


source = Path(sys.argv[1]).read_text()
functions = "\n".join(definition(source, name) for name in (
    "cleanup", "fail", "process_is_live", "lock_owner_alive", "read_lock_ticket",
    "wait_for_update_lock", "acquire_lock",
))

bootstrap = r"""
set -euo pipefail
USAGI_DIR=$1
LOCK_DIR="$USAGI_DIR/update.lock"
ROLE=$2
COORD=$3
LOCK_NODE=""
LOCK_HELD=0
LOCK_ATTEMPTS=0
STAGE_DIR=""
SELECTOR_ACTIVE=0
""" + functions + r"""
trap cleanup EXIT HUP INT TERM
kill() {
    if [ -n "${LIVENESS_ERROR:-}" ]; then
        printf '%s\n' "$LIVENESS_ERROR" >&2
        return 1
    fi
    command kill "$@"
}
mv() {
    local destination="" value
    for value in "$@"; do destination=$value; done
    case "${destination##*/}" in
        owner.*)
            command mv "$@"
            printf '%s\n' "$destination" > "$COORD/node-$ROLE"
            if [ "${PUBLISH_BARRIER:-0}" -eq 1 ]; then
                touch "$COORD/published-$ROLE"
                while [ ! -e "$COORD/publish-$ROLE" ]; do command sleep 0.01; done
            fi
            return
            ;;
        ticket)
            if [ "${TICKET_BARRIER:-0}" -eq 1 ]; then
                touch "$COORD/ticket-ready-$ROLE"
                while [ ! -e "$COORD/ticket-$ROLE" ]; do command sleep 0.01; done
            fi
            ;;
    esac
    command mv "$@"
}
rm() {
    local destination="" value
    for value in "$@"; do destination=$value; done
    case "${destination##*/}" in
        .retired.*)
            if [ "${RETIRE_BARRIER:-0}" -eq 1 ]; then
                printf '%s\n' "$destination" > "$COORD/retired-node-$ROLE"
                touch "$COORD/retired-$ROLE"
                while [ ! -e "$COORD/retire-$ROLE" ]; do command sleep 0.01; done
            fi
            ;;
    esac
    if [ -n "${DEAD_NODE:-}" ] && [ "$destination" = "$DEAD_NODE" ]; then
        touch "$COORD/delete-ready-$ROLE"
        while [ ! -e "$COORD/delete-$ROLE" ]; do command sleep 0.01; done
        command rm "$@"
        touch "$COORD/deleted-$ROLE"
    else
        command rm "$@"
    fi
}
sleep() {
    [ "${FAST_WAIT:-0}" -eq 1 ] && return 0
    touch "$COORD/waiting-$ROLE"
    command sleep "$@"
}
sed() {
    if [ "${FAST_WAIT:-0}" -eq 1 ] && [ "$1" = -n ] && [ "$2" = 1p ]; then
        local line=""
        IFS= read -r line < "$3" || true
        printf '%s\n' "$line"
    else
        command sed "$@"
    fi
}
if [ "${START_BARRIER:-0}" -eq 1 ]; then
    touch "$COORD/started-$ROLE"
    while [ ! -e "$COORD/start-$ROLE" ]; do command sleep 0.01; done
fi
acquire_lock
[ "$LOCK_HELD" -eq 1 ]
mkdir "$COORD/critical" || fail "two live update lock holders"
touch "$COORD/acquired-$ROLE"
while [ ! -e "$COORD/release-$ROLE" ]; do command sleep 0.01; done
rmdir "$COORD/critical"
"""


class Case:
    def __init__(self, root, name):
        self.root = root / name
        self.home = self.root / "home"
        self.coord = self.root / "barriers"
        self.coord.mkdir(parents=True)
        self.children = []

    def launch(self, role, **options):
        env = os.environ.copy()
        env.update({name: str(value) for name, value in options.items()})
        child = subprocess.Popen([
            "/bin/bash", "-c", bootstrap, "install-lock-test", str(self.home),
            role, str(self.coord),
        ], env=env, text=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        self.children.append(child)
        return child

    def await_marker(self, name):
        path = self.coord / name
        deadline = time.monotonic() + 15
        while not path.exists():
            for child in self.children:
                if child.poll() not in (None, 0):
                    raise AssertionError(child.communicate()[1])
            if time.monotonic() >= deadline:
                raise AssertionError(f"{self.root.name}: missing {name}")
            time.sleep(0.005)

    def signal(self, name):
        (self.coord / name).touch()

    def node(self, role):
        return Path((self.coord / f"node-{role}").read_text().strip())

    def finish(self, child, expected=0):
        output, error = child.communicate(timeout=15)
        assert child.returncode == expected, (child.returncode, output, error)
        self.children.remove(child)
        return error

    def close(self):
        for child in self.children:
            if child.poll() is None:
                child.terminate()
            try:
                child.communicate(timeout=15)
            except subprocess.TimeoutExpired:
                child.kill()
                child.communicate(timeout=15)


def concurrent_stale_recovery(root):
    case = Case(root, "stale-recovery-and-equal-tickets")
    try:
        dead = case.home / "update.lock/owner.dead"
        dead.mkdir(parents=True)
        (dead / "pid").write_text("2147483647\n")
        (dead / "choosing").touch()
        children = {
            role: case.launch(role, PUBLISH_BARRIER=1, TICKET_BARRIER=1, DEAD_NODE=dead)
            for role in ("a", "b")
        }
        for role in children:
            case.await_marker(f"published-{role}")
            node = case.node(role)
            assert (node / "pid").is_file() and (node / "choosing").is_file()
            assert not (node / "ticket").exists()
            case.signal(f"publish-{role}")
        for role in children:
            case.await_marker(f"delete-ready-{role}")
        for role in children:
            case.signal(f"delete-{role}")
            case.await_marker(f"deleted-{role}")
            assert all(case.node(owner).is_dir() for owner in children)
        for role in children:
            case.await_marker(f"ticket-ready-{role}")
        for role in children:
            case.signal(f"ticket-{role}")
        winner = min(children, key=lambda role: children[role].pid)
        loser = next(role for role in children if role != winner)
        case.await_marker(f"acquired-{winner}")
        case.await_marker(f"waiting-{loser}")
        assert not (case.coord / f"acquired-{loser}").exists()
        assert all((case.node(role) / "ticket").read_text().strip() == "1"
                   for role in children)
        case.signal(f"release-{winner}")
        case.finish(children[winner])
        case.await_marker(f"acquired-{loser}")
        assert case.node(loser).is_dir(), "cleanup removed another incarnation"
        assert not case.node(winner).exists()
        case.signal(f"release-{loser}")
        case.finish(children[loser])
        assert not list((case.home / "update.lock").glob("owner.*"))
    finally:
        case.close()


def late_lower_pid(root):
    case = Case(root, "late-lower-pid")
    try:
        children = {role: case.launch(role, START_BARRIER=1, PUBLISH_BARRIER=1,
                                     TICKET_BARRIER=1) for role in ("a", "b", "c")}
        for role in children:
            case.await_marker(f"started-{role}")
        lower, first, second = sorted(children, key=lambda role: children[role].pid)
        for role in (first, second):
            case.signal(f"start-{role}")
            case.await_marker(f"published-{role}")
        for role in (first, second):
            case.signal(f"publish-{role}")
            case.await_marker(f"ticket-ready-{role}")
        for role in (first, second):
            case.signal(f"ticket-{role}")
        case.await_marker(f"acquired-{first}")
        case.await_marker(f"waiting-{second}")
        case.signal(f"start-{lower}")
        case.await_marker(f"published-{lower}")
        case.signal(f"publish-{lower}")
        case.await_marker(f"ticket-ready-{lower}")
        case.signal(f"ticket-{lower}")
        case.await_marker(f"waiting-{lower}")
        assert not (case.coord / f"acquired-{lower}").exists()
        assert (case.node(lower) / "ticket").read_text().strip() == "2"
        case.signal(f"release-{first}")
        case.finish(children[first])
        case.await_marker(f"acquired-{second}")
        assert not (case.coord / f"acquired-{lower}").exists()
        case.signal(f"release-{second}")
        case.finish(children[second])
        case.await_marker(f"acquired-{lower}")
        case.signal(f"release-{lower}")
        case.finish(children[lower])
    finally:
        case.close()


def crash_in_choosing(root):
    case = Case(root, "live-choosing-then-crash")
    try:
        crashed = case.launch("a", PUBLISH_BARRIER=1)
        case.await_marker("published-a")
        waiting = case.launch("b")
        case.await_marker("waiting-b")
        assert not (case.coord / "acquired-b").exists(), "live choosing owner was ignored"
        crashed.kill()
        case.finish(crashed, -signal.SIGKILL)
        case.await_marker("acquired-b")
        assert not case.node("a").exists()
        case.signal("release-b")
        case.finish(waiting)
    finally:
        case.close()


def legacy_and_empty_root(root):
    case = Case(root, "legacy-owner")
    try:
        lock = case.home / "update.lock"
        lock.mkdir(parents=True)
        script = r'''
set -eu
printf '%s\n' "$$" > "$1/pid"
touch "$2/legacy-ready"
while [ ! -e "$2/release-legacy" ]; do sleep 0.01; done
rm -rf -- "$1"
'''
        legacy = subprocess.Popen(["/bin/bash", "-c", script, "legacy", str(lock), str(case.coord)])
        case.children.append(legacy)
        case.await_marker("legacy-ready")
        waiting = case.launch("a")
        case.await_marker("waiting-a")
        assert not list(lock.glob("owner.*")), "new owner published before legacy exit"
        case.signal("release-legacy")
        legacy.wait(timeout=15)
        assert legacy.returncode == 0
        case.await_marker("acquired-a")
        case.signal("release-a")
        case.finish(waiting)
        assert lock.is_dir(), "new lock root must remain stable"
        # The root has no pid or participants after normal release.
        unpublished = lock / ".prepare.crashed-without-pid"
        unpublished.mkdir()
        (lock / "pid").write_text("invalid\n")
        again = case.launch("b")
        case.await_marker("acquired-b")
        case.signal("release-b")
        case.finish(again)
        assert unpublished.is_dir(), "unpublished nodes are inert"
        assert lock.stat().st_mode & 0o777 == 0o700
        assert not (lock / "pid").exists(), "legacy metadata must not invite stale root deletion"
    finally:
        case.close()


def invalid_ticket_and_timeout(root):
    for label, ticket, expected in (
        ("overflow", "2147483646", "ticket limit"),
        ("out-of-range", "2147483647", "invalid update lock ticket"),
        ("oversized", "99999999999", "invalid update lock ticket"),
        ("zero", "0", "invalid update lock ticket"),
        ("malformed", "invalid", "invalid update lock ticket"),
        ("timeout", "1", "another usagi update"),
    ):
        case = Case(root, label)
        try:
            live = case.home / "update.lock/owner.fixture"
            live.mkdir(parents=True)
            (live / "pid").write_text(str(os.getpid()) + "\n")
            (live / "ticket").write_text(ticket + "\n")
            child = case.launch("a", FAST_WAIT=1)
            error = case.finish(child, 1)
            assert expected in error, error
            assert live.is_dir(), "failure cleanup removed another owner"
            assert list(live.parent.glob("owner.*")) == [live]
        finally:
            case.close()


def symlinks_and_signal_cleanup(root):
    for label in ("root-symlink", "node-symlink", "signal-while-waiting"):
        case = Case(root, label)
        try:
            outside = case.root / "unrelated"
            outside.mkdir(mode=0o755)
            (outside / "pid").write_text(str(os.getpid()) + "\n")
            (outside / "ticket").write_text("1\n")
            case.home.mkdir()
            lock = case.home / "update.lock"
            if label == "root-symlink":
                lock.symlink_to(outside, target_is_directory=True)
            else:
                lock.mkdir()
                live = lock / "owner.fixture"
                if label == "node-symlink":
                    live.symlink_to(outside, target_is_directory=True)
                else:
                    live.mkdir()
                    (live / "pid").write_text(str(os.getpid()) + "\n")
                    (live / "ticket").write_text("1\n")
            child = case.launch("a")
            if label == "signal-while-waiting":
                case.await_marker("waiting-a")
                assert case.node("a").is_dir()
                child.terminate()
                case.finish(child)
                assert live.is_dir() and not case.node("a").exists()
            else:
                assert "symlink" in case.finish(child, 1)
            assert sorted(path.name for path in outside.iterdir()) == ["pid", "ticket"]
            assert outside.stat().st_mode & 0o777 == 0o755
        finally:
            case.close()


def retiring_a_live_owner_does_not_expose_partial_metadata(root):
    case = Case(root, "atomic-retirement")
    try:
        first = case.launch("a", RETIRE_BARRIER=1)
        case.await_marker("acquired-a")
        second = case.launch("b")
        case.await_marker("waiting-b")
        case.signal("release-a")
        case.await_marker("retired-a")
        retired = Path((case.coord / "retired-node-a").read_text().strip())
        assert retired.is_dir() and not case.node("a").exists()
        assert first.poll() is None, "the retired owner's PID is still alive"
        case.await_marker("acquired-b")
        case.signal("release-b")
        case.finish(second)
        assert retired.is_dir(), "another cleanup must not remove the retired incarnation"
        case.signal("retire-a")
        case.finish(first)
        assert not retired.exists() and (case.home / "update.lock").is_dir()
    finally:
        case.close()


def failed_liveness_probes_preserve_owners_until_timeout(root):
    for label, error in (("permission-denied", "Operation not permitted"),
                         ("unknown-failure", "liveness probe unavailable")):
        for legacy in (False, True):
            case = Case(root, label + ("-legacy" if legacy else "-node"))
            try:
                lock = case.home / "update.lock"
                owner = lock if legacy else lock / "owner.fixture"
                owner.mkdir(parents=True)
                (owner / "pid").write_text(str(os.getpid()) + "\n")
                if not legacy:
                    (owner / "ticket").write_text("1\n")
                child = case.launch("a", FAST_WAIT=1, LIVENESS_ERROR=error)
                assert "another usagi update" in case.finish(child, 1)
                assert owner.is_dir() and (owner / "pid").is_file()
                if not legacy:
                    assert list(lock.glob("owner.*")) == [owner]
                else:
                    assert not list(lock.glob("owner.*"))
            finally:
                case.close()


with tempfile.TemporaryDirectory(dir=sys.argv[2]) as temporary:
    root = Path(temporary)
    for test in (concurrent_stale_recovery, late_lower_pid, crash_in_choosing,
                 legacy_and_empty_root, invalid_ticket_and_timeout,
                 symlinks_and_signal_cleanup,
                 retiring_a_live_owner_does_not_expose_partial_metadata,
                 failed_liveness_probes_preserve_owners_until_timeout):
        test(root)
        print(test.__name__ + ": passed")
