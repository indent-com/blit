blit-gateway(1)

# NAME

blit-gateway - terminal streaming WebSocket gateway

# SYNOPSIS

_blit-gateway_ [*--help*] [*--version*]

# DESCRIPTION

_blit-gateway_ serves the browser UI and proxies WebSocket traffic to a
_blit-server_(1) Unix socket. It handles passphrase authentication and serves
static web assets.

Use it for always-on deployments behind a reverse proxy or as a systemd
service. For local and SSH use, the _blit_(1) CLI embeds equivalent gateway
functionality and is simpler to run.

# OPTIONS

_--help_, _-h_
Print usage and exit.

_--version_, _-V_
Print version and exit.

# ENVIRONMENT

_BLIT_PASS_
Browser passphrase (required).

_BLIT_ADDR_
HTTP/WebSocket listen address (default: _0.0.0.0:3264_).

_BLIT_SOCK_
Upstream server socket. Defaults to _/run/blit/$USER.sock_, then
_$XDG_RUNTIME_DIR/blit.sock_, then _/tmp/blit.sock_.

# SEE ALSO

_blit_(1), _blit-server_(1)
