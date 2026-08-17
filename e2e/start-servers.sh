#!/usr/bin/env bash
# Starts blit server and blit gateway for e2e tests.
# The gateway proxies to the server over a Unix socket.
# Exits when either process exits.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"

# Create a temp directory for the socket
TMPDIR_E2E="${BLIT_E2E_TMPDIR:-$(mktemp -d)}"
export BLIT_SOCK="${TMPDIR_E2E}/blit-test.sock"

# Where a spec can find the server behind the gateway it is driving.  Playwright
# starts this script as its own process tree, so an exported BLIT_SOCK reaches
# the gateway and nothing else — a spec that shells out to the CLI would
# otherwise resolve the *default* socket and quietly interrogate a different
# server.  The file exists only while these servers do, so its absence
# correctly means "somebody else's gateway, use the CLI's own resolution".
SOCK_HANDOFF="${REPO_ROOT}/e2e/.e2e-socket"
printf '%s' "$BLIT_SOCK" >"$SOCK_HANDOFF"

cleanup() {
    # Kill child processes
    kill "$SERVER_PID" "$GATEWAY_PID" 2>/dev/null || true
    wait "$SERVER_PID" "$GATEWAY_PID" 2>/dev/null || true
    rm -f "$SOCK_HANDOFF"
    rm -rf "$TMPDIR_E2E"
}
trap cleanup EXIT INT TERM

# Start blit server
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
