#!/usr/bin/env bash
# Starts blit server and blit gateway for e2e tests.
# The gateway proxies to the server over a Unix socket.
# Exits when either process exits.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"

# Create a temp directory for the socket
TMPDIR_E2E="${BLIT_E2E_TMPDIR:-$(mktemp -d)}"
export BLIT_SOCK="${TMPDIR_E2E}/blit-test.sock"
# Persistent-extension state is single-writer. Keep the test server away from
# any developer server using the platform default database.
export BLIT_EXTENSION_PATH="${TMPDIR_E2E}/extensions.redb"

# The browser connects through the gateway's named-destination mux. Give the
# harness one remote of its own instead of exposing the developer's real
# blit.remotes file (which would make the UI connect to, and drive, those
# servers while a spec believes it is using BLIT_SOCK).
export BLIT_REMOTES="${TMPDIR_E2E}/blit.remotes"
printf 'test = socket:%s\n' "$BLIT_SOCK" >"$BLIT_REMOTES"

# Where a spec can find the server behind the gateway it is driving.  Playwright
# starts this script as its own process tree, so an exported BLIT_SOCK reaches
# the gateway and nothing else — a spec that shells out to the CLI would
# otherwise resolve the *default* socket and quietly interrogate a different
# server.  The file exists only while these servers do, so its absence
# correctly means "somebody else's gateway, use the CLI's own resolution".
SOCK_HANDOFF="${REPO_ROOT}/e2e/.e2e-socket"
printf '%s' "$BLIT_SOCK" >"$SOCK_HANDOFF"

# Where the muster supervisor looks for units, if a spec installs it.  It is
# resolved from the *server's* environment (the extension asks for it over the
# env family), so it has to be set here rather than by the spec — and it has to
# be an empty directory of our own, because the default is the developer's real
# one and a spec that started those units would be starting their work.
export BLIT_MUSTER_DIR="${TMPDIR_E2E}/muster"
mkdir -p "$BLIT_MUSTER_DIR"
MUSTER_HANDOFF="${REPO_ROOT}/e2e/.e2e-muster-dir"
printf '%s' "$BLIT_MUSTER_DIR" >"$MUSTER_HANDOFF"

SERVER_PID=""
GATEWAY_PID=""
cleanup() {
    # Kill child processes
    if [ -n "$SERVER_PID" ]; then
        kill "$SERVER_PID" 2>/dev/null || true
        wait "$SERVER_PID" 2>/dev/null || true
    fi
    if [ -n "$GATEWAY_PID" ]; then
        kill "$GATEWAY_PID" 2>/dev/null || true
        wait "$GATEWAY_PID" 2>/dev/null || true
    fi
    rm -f "$SOCK_HANDOFF" "$MUSTER_HANDOFF"
    rm -rf "$TMPDIR_E2E"
}
trap cleanup EXIT INT TERM

# Start blit server. The Muster spec installs a persistent extension: a
# transient `ext run` ends with the CLI connection that started it, so it is no
# use to a spec that wants an extension still serving when the browser looks.
"${REPO_ROOT}/target/debug/blit" server &
SERVER_PID=$!

# Wait for socket to appear
for i in $(seq 1 30); do
    if [ -S "$BLIT_SOCK" ]; then
        break
    fi
    sleep 0.1
done

if [ ! -S "$BLIT_SOCK" ]; then
    echo "ERROR: blit server socket did not appear at $BLIT_SOCK" >&2
    exit 1
fi

echo "blit server started (pid=$SERVER_PID, socket=$BLIT_SOCK)"

# Start blit gateway
export BLIT_PASSPHRASE="${BLIT_PASSPHRASE:-test-secret}"
export BLIT_ADDR="${BLIT_ADDR:-127.0.0.1:3274}"
"${REPO_ROOT}/target/debug/blit" gateway &
GATEWAY_PID=$!

echo "blit gateway started (pid=$GATEWAY_PID, addr=$BLIT_ADDR)"
echo "READY"

# Wait for either to exit
wait -n "$SERVER_PID" "$GATEWAY_PID" 2>/dev/null || true
