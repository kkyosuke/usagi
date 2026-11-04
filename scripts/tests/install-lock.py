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
released_pid_declaration = next(
    line for line in source.splitlines() if line.startswith("readonly LEGACY_RELEASED_PID=")
)
functions = "\n".join(definition(source, name) for name in (
    "cleanup", "fail", "process_is_live", "read_lock_metadata", "read_lock_pid",
    "lock_owner_alive", "read_lock_ticket",
    "wait_for_update_lock", "publish_legacy_pid", "wait_for_legacy_lock", "acquire_lock",
))
functions += "\n" + definition(source, "read_lock_ticket").replace(
    "read_lock_ticket()", "observed_read_lock_ticket()", 1
)
traps = "\n".join(line for line in source.splitlines() if line.startswith("trap "))

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
""" + released_pid_declaration + "\n" + functions + "\n" + traps + r"""
read_lock_ticket() {
    local value status=0
    value="$(observed_read_lock_ticket "$1")" || status=$?
    if [ -n "${READ_FAILURE_BARRIER_NODE:-}" ] &&
        [ "$1" = "$READ_FAILURE_BARRIER_NODE" ] && [ "$status" -ne 0 ] &&
        [ ! -e "$COORD/read-failed-$ROLE" ]; then
        touch "$COORD/read-failed-$ROLE"
        while [ ! -e "$COORD/read-$ROLE" ]; do command sleep 0.01; done
    fi
    printf '%s\n' "$value"
    return "$status"
}
kill() {
    if [ "$1" = -0 ] && [ "${REUSED_PID:-}" = "$2" ]; then
        # Model an unrelated long-lived process taking a released PID.
        return 0
    fi
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
        pid)
            if [ "${BREADCRUMB_BARRIER:-0}" -eq 1 ]; then
                touch "$COORD/breadcrumb-ready-$ROLE"
                while [ ! -e "$COORD/breadcrumb-$ROLE" ]; do command sleep 0.01; done
            fi
            ;;
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
        pid)
            if [ "${ADMISSION_BARRIER:-0}" -eq 1 ]; then
                touch "$COORD/admission-ready-$ROLE"
                while [ ! -e "$COORD/admission-$ROLE" ]; do command sleep 0.01; done
            fi
            ;;
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
    if [ "${FAST_WAIT:-0}" -eq 1 ]; then
        # Exercise the production deadline boundary without hundreds of
        # forked metadata probes in each failure-policy fixture.
        [ "$LOCK_ATTEMPTS" -ge 596 ] || LOCK_ATTEMPTS=596
        return 0
    fi
    touch "$COORD/waiting-$ROLE"
    command sleep "$@"
}
sed() {
    if [ -n "${PID_READ_ERROR:-}" ] && [ "$3" = "$PID_READ_ERROR" ]; then
        return 1
    fi
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
if [ "${PUBLISH_LEGACY_PID:-0}" -eq 1 ]; then
    if [ "${LEGACY_PUBLISH_BARRIER:-0}" -eq 1 ]; then
        touch "$COORD/legacy-publish-ready-$ROLE"
        while [ ! -e "$COORD/legacy-publish-$ROLE" ]; do command sleep 0.01; done
    fi
    publish_legacy_pid
    touch "$COORD/legacy-published-$ROLE"
fi
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
        ], env=env, text=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            start_new_session=True)
        self.children.append(child)
        return child

    def launch_legacy(self, lock):
        script = r'''
set -eu
printf '%s\n' "$$" > "$1/pid"
touch "$2/legacy-ready"
while [ ! -e "$2/release-legacy" ]; do sleep 0.01; done
rm -rf -- "$1"
'''
        child = subprocess.Popen([
            "/bin/bash", "-c", script, "legacy", str(lock), str(self.coord),
        ], start_new_session=True)
        self.children.append(child)
        return child

    def launch_legacy_acquirer(self):
        child = subprocess.Popen([
            "/bin/bash", str(Path(__file__).parent / "fixtures/install-legacy-lock.sh"),
            str(self.home), str(self.coord),
        ], text=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            start_new_session=True)
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
                try:
                    os.killpg(child.pid, signal.SIGTERM)
                except ProcessLookupError:
                    pass
            try:
                child.communicate(timeout=15)
            except subprocess.TimeoutExpired:
                try:
                    os.killpg(child.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                child.communicate(timeout=15)


def ticket_publication_and_retirement_after_a_failed_read(root):
    for retire in (False, True):
        case = Case(root, "doorway-" + ("retirement" if retire else "publication"))
        try:
            publishing = case.launch("b", TICKET_BARRIER=1)
            case.await_marker("ticket-ready-b")
            scanning = case.launch("a", READ_FAILURE_BARRIER_NODE=case.node("b"))
            case.await_marker("read-failed-a")
            if retire:
                publishing.terminate()
                case.signal("ticket-b")
                case.finish(publishing, 128 + signal.SIGTERM)
                assert not case.node("b").exists()
            else:
                case.signal("ticket-b")
                case.await_marker("waiting-b")
                assert (case.node("b") / "ticket").read_text().strip() == "1"
                assert not (case.node("b") / "choosing").exists()
            case.signal("read-a")
            if not retire:
                case.await_marker("acquired-b")
                case.await_marker("waiting-a")
                assert (case.node("a") / "ticket").read_text().strip() == "2"
                assert not (case.coord / "acquired-a").exists()
                case.signal("release-b")
                case.finish(publishing)
            case.await_marker("acquired-a")
            case.signal("release-a")
            case.finish(scanning)
        finally:
            for role in ("a", "b"):
                for stage in ("read", "ticket", "release"):
                    case.signal(f"{stage}-{role}")
            case.close()


def unreadable_identity_is_unknown_until_timeout(root):
    for legacy in (False, True):
        case = Case(root, "unreadable-pid-" + ("legacy" if legacy else "node"))
        try:
            lock = case.home / "update.lock"
            owner = lock if legacy else lock / "owner.fixture"
            owner.mkdir(parents=True)
            pid = owner / "pid"
            pid.write_text(str(os.getpid()) + "\n")
            if not legacy:
                (owner / "ticket").write_text("1\n")
            child = case.launch("a", FAST_WAIT=1, PID_READ_ERROR=pid)
            assert "another usagi update" in case.finish(child, 1)
            assert owner.is_dir() and pid.read_text().strip() == str(os.getpid())
            if legacy:
                assert not list(lock.glob("owner.*"))
            else:
                assert list(lock.glob("owner.*")) == [owner]
        finally:
            case.close()

    case = Case(root, "unreadable-pid-with-equal-tickets")
    try:
        holder = case.launch("b", PUBLISH_BARRIER=1, TICKET_BARRIER=1)
        case.await_marker("published-b")
        denied = case.launch("a", TICKET_BARRIER=1, FAST_WAIT=1,
                             PID_READ_ERROR=case.node("b") / "pid")
        case.await_marker("ticket-ready-a")
        case.signal("publish-b")
        case.await_marker("ticket-ready-b")
        case.signal("ticket-b")
        case.await_marker("waiting-b")
        case.signal("ticket-a")
        assert "another usagi update" in case.finish(denied, 1)
        case.await_marker("acquired-b")
        assert (case.node("b") / "ticket").read_text().strip() == "1"
        assert case.node("b").is_dir()
        case.signal("release-b")
        case.finish(holder)
    finally:
        for role in ("a", "b"):
            for stage in ("publish", "ticket", "release"):
                case.signal(f"{stage}-{role}")
        case.close()


def malformed_identity_and_special_metadata_remain_closed(root):
    for label, field, shape, legacy in (
        ("empty-pid", "pid", "empty", False),
        ("missing-pid", "pid", "missing", False),
        ("invalid-pid", "pid", "invalid", False),
        ("large-pid", "pid", "large", False),
        ("fifo-pid", "pid", "fifo", False),
        ("fifo-legacy-pid", "pid", "fifo", True),
        ("directory-pid", "pid", "directory", False),
        ("directory-legacy-pid", "pid", "directory", True),
        ("symlink-pid", "pid", "symlink", False),
        ("fifo-ticket", "ticket", "fifo", False),
        ("directory-ticket", "ticket", "directory", False),
        ("symlink-ticket", "ticket", "symlink", False),
    ):
        case = Case(root, label)
        try:
            lock = case.home / "update.lock"
            owner = lock if legacy else lock / "owner.fixture"
            owner.mkdir(parents=True)
            (owner / "pid").write_text(str(os.getpid()) + "\n")
            if not legacy:
                (owner / "ticket").write_text("1\n")
            path = owner / field
            path.unlink()
            if shape == "fifo":
                os.mkfifo(path)
            elif shape == "directory":
                path.mkdir()
            elif shape == "symlink":
                target = owner / "metadata-source"
                target.write_text("1\n")
                path.symlink_to(target)
            elif shape != "missing":
                path.write_text({"empty": "", "invalid": "invalid\n",
                                 "large": "99999999999\n"}[shape])
            child = case.launch("a", FAST_WAIT=1)
            expected = "invalid update lock ticket" if field == "ticket" else "another usagi update"
            assert expected in case.finish(child, 1)
            assert owner.is_dir()
            assert not path.exists() if shape == "missing" else path.exists()
        finally:
            case.close()


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
        legacy = case.launch_legacy(lock)
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
        # The stable root retains a recoverable, never-live marker after release.
        unpublished = lock / ".prepare.crashed-without-pid"
        unpublished.mkdir()
        for index, value in enumerate(("", "0\n", "invalid\n")):
            (lock / "pid").write_text(value)
            role = f"b{index}"
            again = case.launch(role)
            case.await_marker(f"acquired-{role}")
            assert not (lock / "pid").exists(), "active nodes must not publish a stale legacy PID"
            case.signal(f"release-{role}")
            case.finish(again)
        assert unpublished.is_dir(), "unpublished nodes are inert"
        assert lock.stat().st_mode & 0o777 == 0o700
        assert (lock / "pid").read_text().strip() == "2147483647"
    finally:
        case.close()


def legacy_fixture_failure_cleanup_reaps_the_unreleased_child(root):
    case = Case(root, "legacy-failure-cleanup")
    lock = case.home / "update.lock"
    lock.mkdir(parents=True)
    legacy = case.launch_legacy(lock)
    try:
        case.await_marker("legacy-ready")
        owns_group = os.getpgid(legacy.pid) == legacy.pid
        # An earlier assertion can fail before release-legacy is signalled.
        # Use the same failure cleanup as the actual legacy-owner fixture.
        case.close()
        reaped = legacy.poll() is not None
    finally:
        # The regression itself must reap even a broken cleanup implementation.
        if legacy.poll() is None:
            try:
                if os.getpgid(legacy.pid) == legacy.pid:
                    os.killpg(legacy.pid, signal.SIGKILL)
                else:
                    legacy.kill()
            except ProcessLookupError:
                pass
        legacy.wait(timeout=15)
    assert owns_group, "legacy fixture does not own its process group"
    assert reaped, "failure cleanup left the legacy fixture running"


def new_legacy_new_updates_remain_usable(root):
    case = Case(root, "new-legacy-new-updates")
    try:
        first = case.launch("new-before-downgrade")
        case.await_marker("acquired-new-before-downgrade")
        case.signal("release-new-before-downgrade")
        case.finish(first)
        lock = case.home / "update.lock"
        assert lock.is_dir(), "new cleanup must preserve the stable root"
        assert not list(lock.glob("owner.*"))
        legacy = case.launch_legacy_acquirer()
        case.await_marker("acquired-legacy")
        assert (lock / "pid").read_text().strip() == str(legacy.pid)
        waiting = case.launch("new-after-upgrade")
        case.await_marker("waiting-new-after-upgrade")
        assert not (case.coord / "acquired-new-after-upgrade").exists()
        case.signal("release-legacy")
        case.finish(legacy)
        case.await_marker("acquired-new-after-upgrade")
        case.signal("release-new-after-upgrade")
        case.finish(waiting)
        assert lock.is_dir() and not list(lock.glob("owner.*"))
    finally:
        case.close()


def queued_or_cancelled_owners_do_not_enable_legacy_entry(root):
    for cancel_waiter in (False, True):
        case = Case(root, "legacy-during-" + ("cancelled" if cancel_waiter else "queued"))
        try:
            first = case.launch("a")
            case.await_marker("acquired-a")
            second = case.launch("b")
            case.await_marker("waiting-b")
            if cancel_waiter:
                second.terminate()
                case.finish(second, 128 + signal.SIGTERM)
                holder, role = first, "a"
            else:
                case.signal("release-a")
                case.finish(first)
                case.await_marker("acquired-b")
                holder, role = second, "b"
            legacy = case.launch_legacy_acquirer()
            assert "another usagi update" in case.finish(legacy, 1)
            assert case.node(role).is_dir(), "legacy recovery deleted a live owner"
            assert holder.poll() is None and (case.coord / "critical").is_dir()
            case.signal(f"release-{role}")
            case.finish(holder)
        finally:
            case.close()


def late_publication_clears_a_breadcrumb_before_admission(root):
    case = Case(root, "late-breadcrumb")
    try:
        first = case.launch("a", BREADCRUMB_BARRIER=1)
        case.await_marker("acquired-a")
        case.signal("release-a")
        case.await_marker("breadcrumb-ready-a")
        second = case.launch("b")
        case.await_marker("waiting-b")
        assert not (case.coord / "acquired-b").exists()
        case.signal("breadcrumb-a")
        case.finish(first)
        case.await_marker("acquired-b")
        legacy = case.launch_legacy_acquirer()
        assert "another usagi update" in case.finish(legacy, 1)
        assert case.node("b").is_dir()
        case.signal("release-b")
        case.finish(second)
    finally:
        case.close()


def committed_holder_preserves_queued_or_cancelled_owners(root):
    for cancel_waiter in (False, True):
        case = Case(root, "committed-" + ("cancelled" if cancel_waiter else "queued"))
        try:
            first = case.launch("a", PUBLISH_LEGACY_PID=1, LEGACY_PUBLISH_BARRIER=1)
            case.await_marker("legacy-publish-ready-a")
            second = case.launch("b", PUBLISH_LEGACY_PID=1, ADMISSION_BARRIER=1)
            case.await_marker("waiting-b")
            case.signal("legacy-publish-a")
            case.await_marker("legacy-published-a")
            if cancel_waiter:
                second.terminate()
                case.finish(second, 128 + signal.SIGTERM)
                assert (case.home / "update.lock/pid").read_text().strip() == str(first.pid)
                holder, role = first, "a"
            else:
                case.signal("release-a")
                case.finish(first)
                # Pause immediately before B's final metadata removal, while
                # its published owner is still queued and A has fully exited.
                case.await_marker("admission-ready-b")
                holder, role = second, "b"
            legacy = case.launch_legacy_acquirer()
            deadline = time.monotonic() + 15
            while legacy.poll() is None and not (case.coord / "acquired-legacy").exists():
                assert time.monotonic() < deadline, "legacy neither acquired nor timed out"
                time.sleep(0.005)
            if (case.coord / "acquired-legacy").exists():
                case.signal("release-legacy")
                case.finish(legacy)
                raise AssertionError("legacy recovery deleted a live queued owner")
            assert "another usagi update" in case.finish(legacy, 1)
            assert case.node(role).is_dir() and holder.poll() is None
            if not cancel_waiter:
                case.signal("admission-b")
                case.await_marker("legacy-published-b")
            case.signal(f"release-{role}")
            case.finish(holder)
            assert (case.home / "update.lock/pid").read_text().strip() == "2147483647"
            legacy = case.launch_legacy_acquirer()
            case.await_marker("acquired-legacy")
            case.signal("release-legacy")
            case.finish(legacy)
            assert not (case.home / "update.lock").exists()
        finally:
            case.close()


def solo_cleanup_does_not_confuse_pid_reuse_with_an_update(root):
    for restricted_probe in (False, True):
        case = Case(root, "solo-pid-reuse-" + ("restricted" if restricted_probe else "normal"))
        try:
            first = case.launch("a", PUBLISH_LEGACY_PID=1)
            case.await_marker("legacy-published-a")
            case.signal("release-a")
            case.finish(first)
            options = {"LIVENESS_ERROR": "Operation not permitted"} if restricted_probe else {}
            second = case.launch("b", FAST_WAIT=1, REUSED_PID=first.pid, **options)
            case.await_marker("acquired-b")
            case.signal("release-b")
            case.finish(second)
            legacy = case.launch_legacy_acquirer()
            case.await_marker("acquired-legacy")
            case.signal("release-legacy")
            case.finish(legacy)
            assert not (case.home / "update.lock").exists()
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
    for label, interrupted in (("root-symlink", None), ("node-symlink", None),
                               ("hup-while-waiting", signal.SIGHUP),
                               ("int-while-waiting", signal.SIGINT),
                               ("term-while-waiting", signal.SIGTERM)):
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
            if interrupted is not None:
                case.await_marker("waiting-a")
                assert case.node("a").is_dir()
                child.send_signal(interrupted)
                case.finish(child, 128 + interrupted)
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
                 legacy_and_empty_root,
                 new_legacy_new_updates_remain_usable,
                 queued_or_cancelled_owners_do_not_enable_legacy_entry,
                 late_publication_clears_a_breadcrumb_before_admission,
                 committed_holder_preserves_queued_or_cancelled_owners,
                 solo_cleanup_does_not_confuse_pid_reuse_with_an_update,
                 legacy_fixture_failure_cleanup_reaps_the_unreleased_child,
                 invalid_ticket_and_timeout,
                 symlinks_and_signal_cleanup,
                 retiring_a_live_owner_does_not_expose_partial_metadata,
                 failed_liveness_probes_preserve_owners_until_timeout,
                 ticket_publication_and_retirement_after_a_failed_read,
                 unreadable_identity_is_unknown_until_timeout,
                 malformed_identity_and_special_metadata_remain_closed):
        test(root)
        print(test.__name__ + ": passed")
