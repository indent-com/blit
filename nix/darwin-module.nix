self:
{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.services.blit;
  inherit (lib)
    mkEnableOption
    mkOption
    types
    mkIf
    ;
in
{
  options.services.blit = {
    enable = mkEnableOption "blit terminal multiplexer";

    package = mkOption {
      type = types.package;
      default = self.packages.${pkgs.system}.blit-server;
      defaultText = "self.packages.\${system}.blit-server";
      description = "The blit-server package to use.";
    };

    shell = mkOption {
      type = types.nullOr types.str;
      default = null;
      example = "/run/current-system/sw/bin/fish";
      description = "Shell to spawn for new PTYs. Defaults to the user's login shell.";
    };

    scrollback = mkOption {
      type = types.int;
      default = 10000;
      description = "Scrollback buffer size in rows per PTY.";
    };

    socketPath = mkOption {
      type = types.nullOr types.str;
      default = null;
      description = "Unix socket path for blit-server. Defaults to $TMPDIR/blit.sock.";
    };

    gateways = mkOption {
      type = types.attrsOf (
        types.submodule {
          options = {
            port = mkOption {
              type = types.port;
              default = 3264;
              description = "Port to listen on.";
            };
            addr = mkOption {
              type = types.str;
              default = "127.0.0.1";
              description = "Address to bind to.";
            };
            passFile = mkOption {
              type = types.path;
              description = "File containing BLIT_PASSPHRASE=<passphrase>.";
            };
            fontDirs = mkOption {
              type = types.listOf types.str;
              default = [ ];
              example = [
                "/Library/Fonts"
                "~/Library/Fonts"
              ];
              description = "Extra font directories to search.";
            };
            quic = mkOption {
              type = types.bool;
              default = false;
              description = "Enable WebTransport (QUIC/HTTP3) alongside WebSocket.";
            };
            tlsCert = mkOption {
              type = types.nullOr types.path;
              default = null;
              description = "PEM certificate file for WebTransport TLS. Auto-generated if null.";
            };
            tlsKey = mkOption {
              type = types.nullOr types.path;
              default = null;
              description = "PEM private key file for WebTransport TLS. Auto-generated if null.";
            };
            remoteFile = mkOption {
              type = types.nullOr types.path;
              default = null;
              example = "/etc/blit/remotes";
              description = ''
                Path to a <literal>blit.remotes</literal>-format file listing
                named destinations for this gateway.  When unset, the gateway
                uses <literal>~/.config/blit/blit.remotes</literal> (the
                user's default remotes file, writable via
                <literal>blit remote add</literal>).  The file is
                live-reloaded on change; no gateway restart required.
              '';
            };
            storeConfig = mkOption {
              type = types.bool;
              default = false;
              description = "Sync browser settings to ~/.config/blit/blit.conf.";
            };
            webrtcProxy = mkOption {
              type = types.bool;
              default = false;
              description = ''
                Enable gateway-side WebRTC proxying for
                <literal>share:</literal> remotes (<literal>BLIT_GATEWAY_WEBRTC=1</literal>).
                When enabled, the gateway connects to the signaling hub as a
                WebRTC consumer and bridges <literal>share:</literal> sessions
                to browsers over WebSocket/WebTransport.
                Without this, <literal>share:</literal> entries in
                <literal>blit.remotes</literal> are ignored by the gateway.
              '';
            };
            hub = mkOption {
              type = types.nullOr types.str;
              default = null;
              example = "hub.blit.sh";
              description = ''
                Signaling hub URL for <literal>share:</literal> remotes
                (sets <literal>BLIT_HUB</literal>).
                Only used when <option>webrtcProxy</option> is enabled.
                Defaults to <literal>hub.blit.sh</literal>.
              '';
            };
            package = mkOption {
              type = types.package;
              default = self.packages.${pkgs.system}.blit-gateway;
              defaultText = "self.packages.\${system}.blit-gateway";
              description = "The blit-gateway package to use.";
            };
          };
        }
      );
      default = { };
      description = "Named blit-gateway instances.";
    };

    forwarders = mkOption {
      type = types.attrsOf (
        types.submodule {
          options = {
            passFile = mkOption {
              type = types.path;
              description = "File containing BLIT_PASSPHRASE=<passphrase>.";
            };
            hub = mkOption {
              type = types.nullOr types.str;
              default = null;
              description = "Signaling hub URL. Defaults to hub.blit.sh.";
            };
            quiet = mkOption {
              type = types.bool;
              default = true;
              description = "Don't print the sharing URL.";
            };
            verbose = mkOption {
              type = types.bool;
              default = false;
              description = "Print detailed connection diagnostics to stderr.";
            };
            verboseWebrtc = mkOption {
              type = types.bool;
              default = false;
              description = "Enable WebRTC-level tracing (BLIT_WEBRTC_VERBOSE=1): ICE candidates, STUN/TURN results, SDP exchange, and DataChannel events.";
            };
            package = mkOption {
              type = types.package;
              default = self.packages.${pkgs.system}.blit-webrtc-forwarder;
              defaultText = "self.packages.\${system}.blit-webrtc-forwarder";
              description = "The blit-webrtc-forwarder package to use.";
            };
          };
        }
      );
      default = { };
      description = "Named blit-webrtc-forwarder instances sharing blit-server sessions via WebRTC.";
    };
  };

  config = mkIf cfg.enable {
    launchd.user.agents = {
      blit = {
        serviceConfig = {
          Label = "com.blit.server";
          ProgramArguments = [
            "/bin/sh"
            "-lc"
            ''[ -n "$LANG" ] || export LANG="$(defaults read -g AppleLocale 2>/dev/null | sed 's/@.*//' || echo en_US).UTF-8"; exec ${cfg.package}/bin/blit-server''
          ];
          EnvironmentVariables = {
            BLIT_SCROLLBACK = toString cfg.scrollback;
          }
          // lib.optionalAttrs (cfg.socketPath != null) {
            BLIT_SOCK = cfg.socketPath;
          }
          // lib.optionalAttrs (cfg.shell != null) {
            SHELL = cfg.shell;
          };
          WorkingDirectory = "~";
          RunAtLoad = true;
          KeepAlive = true;
          StandardOutPath = "/tmp/blit-server.log";
          StandardErrorPath = "/tmp/blit-server.log";
        };
      };
    }
    // builtins.listToAttrs (
      lib.mapAttrsToList (name: gw: {
        name = "blit-gateway-${name}";
        value =
          let
            effectiveRemoteFile = gw.remoteFile;
          in
          {
            serviceConfig = {
              Label = "com.blit.gateway.${name}";
              ProgramArguments = [
                "/bin/sh"
                "-lc"
                ". ${gw.passFile} && exec ${gw.package}/bin/blit-gateway"
              ];
              EnvironmentVariables = {
                BLIT_ADDR = "${gw.addr}:${toString gw.port}";
              }
              // lib.optionalAttrs (effectiveRemoteFile != null) {
                BLIT_REMOTES = effectiveRemoteFile;
              }
              // lib.optionalAttrs (gw.fontDirs != [ ]) {
                BLIT_FONT_DIRS = lib.concatStringsSep ":" gw.fontDirs;
              }
              // lib.optionalAttrs gw.storeConfig {
                BLIT_STORE_CONFIG = "1";
              }
              // lib.optionalAttrs gw.quic {
                BLIT_QUIC = "1";
              }
              // lib.optionalAttrs (gw.tlsCert != null) {
                BLIT_TLS_CERT = gw.tlsCert;
              }
              // lib.optionalAttrs (gw.tlsKey != null) {
                BLIT_TLS_KEY = gw.tlsKey;
              }
              // lib.optionalAttrs gw.webrtcProxy {
                BLIT_GATEWAY_WEBRTC = "1";
              }
              // lib.optionalAttrs (gw.hub != null) {
                BLIT_HUB = gw.hub;
              };
              RunAtLoad = true;
              KeepAlive = true;
              StandardOutPath = "/tmp/blit-gateway-${name}.log";
              StandardErrorPath = "/tmp/blit-gateway-${name}.log";
            };
          };
      }) cfg.gateways
    )
    // builtins.listToAttrs (
      lib.mapAttrsToList (name: fwd: {
        name = "blit-webrtc-forwarder-${name}";
        value = {
          serviceConfig = {
            Label = "com.blit.webrtc-forwarder.${name}";
            ProgramArguments = [
              "/bin/sh"
              "-lc"
              (
                ". ${fwd.passFile} && exec ${fwd.package}/bin/blit-webrtc-forwarder"
                + lib.optionalString fwd.quiet " --quiet"
                + lib.optionalString fwd.verbose " --verbose"
              )
            ];
            EnvironmentVariables =
              { }
              // lib.optionalAttrs (cfg.socketPath != null) {
                BLIT_SOCK = cfg.socketPath;
              }
              // lib.optionalAttrs (fwd.hub != null) {
                BLIT_HUB = fwd.hub;
              }
              // lib.optionalAttrs fwd.verboseWebrtc {
                BLIT_WEBRTC_VERBOSE = "1";
              };
            RunAtLoad = true;
            KeepAlive = true;
            StandardOutPath = "/tmp/blit-webrtc-forwarder-${name}.log";
            StandardErrorPath = "/tmp/blit-webrtc-forwarder-${name}.log";
          };
        };
      }) cfg.forwarders
    );
  };
}
