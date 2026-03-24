blit-server(1)

# NAME

blit-server - terminal streaming server

# SYNOPSIS

_blit-server_ [*--socket* _PATH_] [_PATH_]

# DESCRIPTION

_blit-server_ multiplexes PTYs on a Unix socket. It tracks terminal state using
the wezterm parser, diffs cell grids into LZ4-compressed binary frames, and
publishes updates to connected clients.

Supports systemd socket activation via _LISTEN_FDS=1_.

# OPTIONS

_--socket_ _PATH_
Unix socket path. Can also be given as a positional argument.

_--help_, _-h_
Print usage and exit.

_--version_, _-V_
Print version and exit.

# ENVIRONMENT

_BLIT_SOCK_
Unix socket path. Defaults to _$XDG_RUNTIME_DIR/blit.sock_ or
_/tmp/blit.sock_.

_SHELL_
Shell to spawn for new PTYs (default: _/bin/sh_).

_BLIT_SCROLLBACK_
Scrollback buffer rows per PTY (default: 10000).

# SEE ALSO

_blit_(1), _blit-gateway_(1)
