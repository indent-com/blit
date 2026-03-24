blit(1)

# NAME

blit - terminal streaming client

# SYNOPSIS

_blit_ [OPTIONS] [HOST]

# DESCRIPTION

_blit_ connects to a _blit-server_(1) instance and provides a terminal
interface, either in the browser (default) or in the current terminal
(_--console_ mode).

Without arguments, it connects to the local Unix socket. A bare hostname is
treated as an SSH target: blit opens an SSH tunnel and starts an embedded HTTP
gateway on a random loopback port, then opens the browser.

In browser mode, each tab gets its own connection to the server.

# OPTIONS

_--console_
Render directly to the terminal using the ANSI renderer instead of
opening a browser.

_--socket_ _PATH_
Connect to a specific Unix socket.

_--tcp_ _HOST:PORT_
Connect via raw TCP.

_--help_, _-h_
Print usage and exit.

_--version_, _-V_
Print version and exit.

# ENVIRONMENT

_BLIT_SOCK_
Unix socket path. Defaults to _/run/blit/$USER.sock_, then
_$XDG_RUNTIME_DIR/blit.sock_, then _/tmp/blit.sock_.

_BLIT_DISPLAY_FPS_
Advertised client display rate in console mode (default: 240, clamped
10–1000).

# SEE ALSO

_blit-server_(1), _blit-gateway_(1)
