blit-server(1)

# NAME

blit-server - terminal streaming server

# SYNOPSIS

_blit-server_ [*--socket* _PATH_] [*--fd-channel* _FD_] [*--shell-flags* _FLAGS_] [_PATH_]

# DESCRIPTION

_blit-server_ multiplexes PTYs on a Unix socket. It tracks terminal state,
diffs cell grids into LZ4-compressed binary frames, and publishes updates to
connected clients.

Supports systemd socket activation via _LISTEN_FDS=1_.

# OPTIONS

_--socket_ _PATH_
Unix socket path. Can also be given as a positional argument.

_--fd-channel_ _FD_
Accept clients via fd-passing on the given file descriptor number.

_--shell-flags_ _FLAGS_
Flags passed to the shell when spawning PTYs (default: _li_).

_--help_, _-h_
Print usage and exit.

_--version_, _-V_
Print version and exit.

# ENVIRONMENT

_BLIT_SOCK_
Unix socket path. Defaults to _$TMPDIR/blit.sock_, then
_$XDG_RUNTIME_DIR/blit.sock_, then _/tmp/blit-$USER.sock_, then
_/tmp/blit.sock_.

_SHELL_
Shell to spawn for new PTYs (default: _/bin/sh_).

_BLIT_SHELL_FLAGS_
Shell flags passed to the shell (default: _li_). Overridden by
_--shell-flags_.

_BLIT_SCROLLBACK_
Scrollback buffer rows per PTY (default: 10000).

_BLIT_FD_CHANNEL_
File descriptor number for fd-passing channel. Overridden by
_--fd-channel_.

# SEE ALSO

_blit_(1), _blit-gateway_(1)
