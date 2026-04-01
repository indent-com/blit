blit-webrtc-forwarder(1)

# NAME

blit-webrtc-forwarder - forward a blit-server session over WebRTC

# SYNOPSIS

_blit-webrtc-forwarder_ *--socket* _PATH_ *--passphrase* _PASSPHRASE_ [*--hub* _URL_] [*--url* _TEMPLATE_] [*--quiet*] [*--verbose*]

# DESCRIPTION

_blit-webrtc-forwarder_ connects to a _blit-server_(1) Unix socket and
bridges it to browsers over WebRTC data channels. It handles signaling,
STUN/TURN NAT traversal, and peer-to-peer connections.

For most use cases, _blit share_ is simpler — it runs the forwarder
in-process and auto-starts a server if needed. The standalone binary is
for custom deployments where the server is managed separately.

# OPTIONS

_--socket_ _PATH_
Path to the blit-server Unix socket (required).

_--passphrase_ _PASSPHRASE_
Session passphrase. Used to derive the Ed25519 signing key for
signaling authentication. Can also be set via _BLIT_PASSPHRASE_.

_--hub_ _URL_
Signaling hub URL (default: _hub.blit.sh_).

_--url_ _TEMPLATE_
URL template for the sharing link. Use _{secret}_ as a placeholder for
the passphrase (default: _https://blit.sh/#{secret}_).

_--quiet_
Don't print the sharing URL.

_--verbose_
Print detailed connection diagnostics (ICE negotiation, STUN/TURN
results, peer events) to stderr.

_--help_, _-h_
Print usage and exit.

_--version_, _-V_
Print version and exit.

# ENVIRONMENT

_BLIT_HUB_
Signaling hub URL (default: _hub.blit.sh_). Overridden by _--hub_.

_BLIT_PASSPHRASE_
Session passphrase. Overridden by _--passphrase_.

# SEE ALSO

_blit_(1), _blit-server_(1)
