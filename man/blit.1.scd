blit(1)

# NAME

blit - terminal streaming client

# SYNOPSIS

_blit_ [OPTIONS] [HOST]

_blit_ [OPTIONS] _list_

_blit_ [OPTIONS] _start_ [_--tag_ _TAG_] [_--rows_ _N_] [_--cols_ _N_] [_--wait_ _--timeout_ _N_] [_COMMAND..._]

_blit_ [OPTIONS] _show_ [_--ansi_] [_--rows_ _N_] [_--cols_ _N_] _ID_

_blit_ [OPTIONS] _history_ [_--ansi_] [_--from-start_ _N_ | _--from-end_ _N_] [_--limit_ _N_] [_--rows_ _N_] [_--cols_ _N_] _ID_

_blit_ [OPTIONS] _send_ _ID_ _TEXT_

_blit_ [OPTIONS] _restart_ _ID_

_blit_ [OPTIONS] _wait_ _--timeout_ _N_ [_--pattern_ _REGEX_] _ID_

_blit_ [OPTIONS] _close_ _ID_

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

_--port_ _PORT_
Bind the embedded browser gateway to a specific port instead of
choosing one at random.

_--socket_ _PATH_, _-s_ _PATH_
Connect to a specific Unix socket.

_--tcp_ _HOST:PORT_
Connect via raw TCP.

_--ssh_ _HOST_
Connect to a remote _blit-server_ over SSH. The remote socket path is
resolved using the same cascade as _BLIT_SOCK_ (see below), evaluated on
the remote host as the SSH user.

_--help_, _-h_
Print usage and exit.

_--version_, _-V_
Print version and exit.

# SUBCOMMANDS

The following subcommands are designed for programmatic and LLM agent use.
Output is plain text with no decoration. Errors go to stderr; non-zero exit
on failure. The connection options (_--socket_, _--tcp_, _--ssh_) are accepted
by all subcommands.

_list_
List all terminal sessions. Output is TSV with columns: ID, TAG, TITLE,
STATUS.

_start_ [_--tag_ _TAG_] [_--rows_ _N_] [_--cols_ _N_] [_--wait_ _--timeout_ _N_] [_COMMAND..._]
Start a new PTY and print its session ID. If _COMMAND_ is omitted, the
user's default shell is started. The default size is 24 rows by 80
columns. With _--wait_ _--timeout_ _N_, block until the process exits
(or until _N_ seconds elapse). Exit code is the process's exit code, or
124 on timeout.

_show_ [_--ansi_] [_--rows_ _N_] [_--cols_ _N_] _ID_
Dump the current visible terminal text for session _ID_. With _--ansi_,
include ANSI color and style escape sequences. _--rows_ and _--cols_
resize the terminal before capturing; defaults are 24 rows and 80
columns when either flag is given.

_history_ [_--ansi_] [_--from-start_ _N_ | _--from-end_ _N_] [_--limit_ _N_] [_--rows_ _N_] [_--cols_ _N_] _ID_
Dump scrollback and viewport for session _ID_. _--from-start_ _N_ begins
_N_ lines from the top (oldest = 0). _--from-end_ _N_ begins _N_ lines
from the bottom (newest = 0). _--limit_ caps the number of lines
returned. With _--ansi_, include ANSI escape sequences. _--rows_ and
_--cols_ resize the terminal before reading; defaults are 24 rows and 80
columns when either flag is given.

_send_ _ID_ _TEXT_
Send keystrokes to session _ID_. _TEXT_ supports C-style escapes
(_\\n_, _\\t_, _\\x1b_). Use _-_ to read from stdin. Returns an error
if the session has already exited.

_wait_ _--timeout_ _N_ [_--pattern_ _REGEX_] _ID_
Block until session _ID_ exits or _N_ seconds elapse. Prints the exit
status (e.g. _exited(0)_, _signal(9)_) to stdout and exits with the
process's exit code. Exit code 124 on timeout. With _--pattern_, instead
match _REGEX_ against new output produced after the wait begins; on
match, print the matching line and exit 0.

_restart_ _ID_
Restart an exited session. The original command, terminal size, and tag
are preserved. The session ID is reused but terminal scrollback is *not*
cleared — old output persists. Fails if the session is still running.

_close_ _ID_
Close and remove session _ID_.

# ENVIRONMENT

_BLIT_SOCK_
Unix socket path. Defaults to _$TMPDIR/blit.sock_, then
_/tmp/blit-$USER.sock_, then _/run/blit/$USER.sock_, then
_$XDG_RUNTIME_DIR/blit.sock_, then _/tmp/blit.sock_.

_BLIT_DISPLAY_FPS_
Advertised client display rate in console mode (default: 240, clamped
10–1000).

# SEE ALSO

_blit-server_(1), _blit-gateway_(1)
