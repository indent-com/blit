blit(1)

# NAME

blit - terminal streaming client

# SYNOPSIS

_blit_ [OPTIONS] _open_ [_--console_ | _--port_ _PORT_]

_blit_ [OPTIONS] _list_

_blit_ [OPTIONS] _start_ [_--tag_ _TAG_] [_--rows_ _N_] [_--cols_ _N_] [_--wait_ _--timeout_ _N_] [_COMMAND..._]

_blit_ [OPTIONS] _show_ [_--ansi_] [_--rows_ _N_] [_--cols_ _N_] _ID_

_blit_ [OPTIONS] _history_ [_--ansi_] [_--from-start_ _N_ | _--from-end_ _N_] [_--limit_ _N_] [_--rows_ _N_] [_--cols_ _N_] _ID_

_blit_ [OPTIONS] _send_ _ID_ _TEXT_

_blit_ [OPTIONS] _restart_ _ID_

_blit_ [OPTIONS] _wait_ _--timeout_ _N_ [_--pattern_ _REGEX_] _ID_

_blit_ [OPTIONS] _kill_ _ID_ [_SIGNAL_]

_blit_ [OPTIONS] _close_ _ID_

_blit_ [OPTIONS] _server_ [_--shell-flags_ _FLAGS_] [_--scrollback_ _N_] [_--fd-channel_ _FD_]

_blit_ [OPTIONS] _share_ [_--passphrase_ _PASSPHRASE_] [_--quiet_] [_--verbose_]

_blit_ _learn_

_blit_ _install_ _--ssh_ _HOST_

_blit_ _upgrade_

# DESCRIPTION

_blit_ is a terminal streaming tool for browsers and AI agents.

_blit open_ connects to a _blit-server_(1) instance and opens the terminal
UI in a browser. With _--console_, it renders directly to the terminal
instead.

Most subcommands (_list_, _start_, _show_, _send_, etc.) are designed for
programmatic use by scripts and LLM agents. Run _blit learn_ to print the
full CLI reference.

# OPTIONS

_--socket_ _PATH_, _-s_ _PATH_
Connect to a specific Unix socket. When combined with _--ssh_, specifies
the socket path on the remote host instead of the default search order.

_--tcp_ _HOST:PORT_
Connect via raw TCP.

_--ssh_ _HOST_
Connect to a remote _blit-server_ over SSH. The remote socket path is
resolved using the same cascade as _BLIT_SOCK_ (see below), evaluated on
the remote host as the SSH user. If no server is running, _blit server_
(or _blit-server_) is started automatically on the remote host. Can be
combined with _--socket_ to target a specific remote socket path.

_--passphrase_ _PASSPHRASE_
Connect via WebRTC to a shared session using the given passphrase.

_--hub_ _URL_
Signaling hub URL for WebRTC sharing (default: _hub.blit.sh_). Can also
be set via _BLIT_HUB_.

_--help_, _-h_
Print usage and exit.

_--version_, _-V_
Print version and exit.

# SUBCOMMANDS

The following subcommands are designed for programmatic and LLM agent use.
Output is plain text with no decoration. Errors go to stderr; non-zero exit
on failure. The connection options (_--socket_, _--tcp_, _--ssh_) are accepted
by all subcommands.

_open_ [_--console_ | _--port_ _PORT_]
Open the terminal UI. By default, opens a browser. With _--console_,
renders directly to the terminal. _--port_ binds the embedded gateway to
a specific port. _--console_ and _--port_ are mutually exclusive.

_list_
List all terminal sessions. Output is TSV with columns: ID, TAG, TITLE,
COMMAND, STATUS.

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
are preserved. The session ID is reused but terminal scrollback is _not_
cleared — old output persists. Fails if the session is still running.

_kill_ _ID_ [_SIGNAL_]
Send a signal to the leader process of session _ID_. Accepts signal names
(_TERM_, _KILL_, _INT_, _HUP_, _USR1_, etc.) or numbers (_9_, _15_).
Defaults to _TERM_. Returns an error if the session has already exited.

_close_ _ID_
Close and remove session _ID_.

_server_ [_--shell-flags_ _FLAGS_] [_--scrollback_ _N_] [_--fd-channel_ _FD_]
Run the blit terminal multiplexer server in-process. The server listens
on the Unix socket specified by _--socket_ (or the default path). This is
equivalent to running _blit-server_(1) as a standalone binary.

_share_ [_--passphrase_ _PASSPHRASE_] [_--quiet_] [_--verbose_]
Share a terminal session via WebRTC. If no server is running at the
default socket path, one is started automatically in-process. A sharing
URL is printed to stdout. The passphrase can also be set via the
_BLIT_PASSPHRASE_ environment variable. If omitted, a random passphrase
is generated. With _--verbose_, detailed connection diagnostics (ICE
negotiation, STUN/TURN results, peer events) are printed to stderr.

_install_
Install _blit_ on a remote host via SSH. Requires _--ssh_; other
transports are rejected. Detects the remote OS via _uname_: on Linux and
macOS the curl/wget install script is run; on Windows the PowerShell
installer is used. The installer's stdin and stdout are connected to
the local terminal for interactive prompts.

_learn_
Print the full CLI reference — a usage guide for scripts and LLM agents.

_upgrade_
Upgrade _blit_ to the latest version. Downloads and runs the install
script from _https://install.blit.sh_, installing into the same
directory as the currently running binary.

# ENVIRONMENT

_BLIT_SOCK_
Unix socket path. Defaults to _$TMPDIR/blit.sock_, then
_/tmp/blit-$USER.sock_, then _/run/blit/$USER.sock_, then
_$XDG_RUNTIME_DIR/blit.sock_, then _/tmp/blit.sock_.

_BLIT_HUB_
Signaling hub URL for WebRTC sharing (default: _hub.blit.sh_).
Overridden by _--hub_.

_BLIT_PASSPHRASE_
Passphrase for _share_. Overridden by _--passphrase_.

_BLIT_DISPLAY_FPS_
Advertised client display rate in console mode (default: 240, clamped
10–1000).

# SEE ALSO

_blit-server_(1), _blit-gateway_(1), _blit-webrtc-forwarder_(1)
