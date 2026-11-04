# Frozen lock protocol from v4.9.2 scripts/install.sh. This intentionally keeps
# the old mkdir/PID recovery contract to test a downgrade followed by an update.
set -euo pipefail
USAGI_DIR=$1
LOCK_DIR="$USAGI_DIR/update.lock"
COORD=$2
LOCK_HELD=0

cleanup() {
    local status=$?
    if [ "$LOCK_HELD" -eq 1 ] && [ -d "$LOCK_DIR" ]; then
        rm -rf -- "$LOCK_DIR"
    fi
    exit "$status"
}
trap cleanup EXIT HUP INT TERM

fail() {
    echo "Error: $*" >&2
    exit 1
}

acquire_lock() {
    local attempt=0 owner=""
    mkdir -p -- "$USAGI_DIR"
    chmod 700 "$USAGI_DIR"
    while ! mkdir -m 700 "$LOCK_DIR" 2>/dev/null; do
        if [ -f "$LOCK_DIR/pid" ]; then
            owner="$(sed -n '1p' "$LOCK_DIR/pid" 2>/dev/null || true)"
        fi
        case "$owner" in
            ''|*[!0-9]*) ;;
            *)
                if ! kill -0 "$owner" 2>/dev/null; then
                    rm -rf -- "$LOCK_DIR"
                    owner=""
                    continue
                fi
                ;;
        esac
        attempt=$((attempt + 1))
        [ "$attempt" -lt 600 ] || fail "another usagi update is still running"
        sleep 0.1
    done
    LOCK_HELD=1
    printf '%s\n' "$$" > "$LOCK_DIR/pid"
}

# Only the historical wait interval is shortened; a broken empty-root recovery
# still exhausts the unchanged 600-attempt bound.
sleep() { :; }
acquire_lock
mkdir "$COORD/critical" || fail "two live update lock holders"
touch "$COORD/acquired-legacy"
while [ ! -e "$COORD/release-legacy" ]; do command sleep 0.01; done
rmdir "$COORD/critical"
