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

When _BLIT_QUIC=1_, the gateway also listens for WebTransport (HTTP/3)
connections on the same address, requiring TLS certificates.

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
	Upstream server socket. Defaults to _$TMPDIR/blit.sock_, then
	_$XDG_RUNTIME_DIR/blit.sock_, then _/tmp/blit-$USER.sock_, then
	_/tmp/blit.sock_.

_BLIT_FONT_DIRS_
	Colon-separated list of extra directories to scan for fonts.

_BLIT_CORS_
	CORS origin for font routes. Set to _*_ or a specific origin.

_BLIT_QUIC_
	Set to _1_ to enable WebTransport (QUIC/HTTP3).

_BLIT_TLS_CERT_
	Path to a PEM certificate file for WebTransport. Auto-generated if
	unset.

_BLIT_TLS_KEY_
	Path to a PEM private key file for WebTransport. Auto-generated if
	unset.

# SEE ALSO

_blit_(1), _blit-server_(1)
