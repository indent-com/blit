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

  # blit server loads GPU encoder libraries via dlopen at runtime:
  #   VA-API:  libva.so.2, libva-drm.so.2  (from pkgs.libva)
  #   NVENC:   libcuda.so.1, libnvidia-encode.so.1  (from the GPU driver)
  # On NixOS these live under /nix/store and are not in the default
  # ld.so search path.  addDriverRunpath.driverLink is the NixOS-managed
  # symlink farm (/run/opengl-driver) for the active GPU driver (NVIDIA,
  # Mesa, etc.) and covers NVENC, CUDA, and VA-API backend drivers.
  gpuLibSearchPath = lib.makeLibraryPath (cfg.gpuLibraries ++ [ pkgs.addDriverRunpath.driverLink ]);
in
{
  options.services.blit = {
    enable = mkEnableOption "blit terminal multiplexer";

    package = mkOption {
      type = types.package;
      default = self.packages.${pkgs.system}.blit;
      defaultText = "self.packages.\${system}.blit";
      description = "The blit package to use.";
    };

    users = mkOption {
      type = types.listOf types.str;
      default = [ ];
      example = [
        "alice"
        "bob"
      ];
      description = ''
        Users to enable blit for. Each user gets a socket-activated
        blit server instance at /run/blit/<user>.sock.
      '';
    };

    shell = mkOption {
      type = types.nullOr types.str;
      default = null;
      example = "/run/current-system/sw/bin/bash";
      description = "Shell to spawn for new PTYs. Defaults to the user's login shell.";
    };

    scrollback = mkOption {
      type = types.int;
      default = 10000;
      description = "Scrollback buffer size in rows per PTY.";
    };

    audio = {
      enable = mkEnableOption "audio forwarding (PipeWire capture + Opus)";

      bitrate = mkOption {
        type = types.int;
        default = 64000;
        description = "Opus encoder bitrate in bits/sec.";
      };
    };

    gpuLibraries = mkOption {
      type = types.listOf types.package;
      default = lib.optionals pkgs.stdenv.isLinux [
        pkgs.libva
        pkgs.libgbm
        pkgs.vulkan-loader
      ];
      defaultText = "[ pkgs.libva pkgs.libgbm pkgs.vulkan-loader ] (Linux only)";
      description = ''
        Libraries to make available to blit server via LD_LIBRARY_PATH
        for hardware-accelerated video encoding and GPU compositing.
        blit server loads VA-API, Vulkan, and GBM via dlopen at
        runtime; on NixOS these shared objects are not in the default
        search path.

        Set to an empty list to disable hardware acceleration and use
        only software encoders (openh264, rav1e).
      '';
    };

    gateways = mkOption {
      type = types.attrsOf (
        types.submodule {
          options = {
            user = mkOption {
              type = types.nullOr types.str;
              default = null;
              description = ''
                User to run the gateway process as, and whose
                <literal>blit-server@&lt;user&gt;.socket</literal> to depend on.
                Required when not using <option>remoteFile</option>.
              '';
            };
            port = mkOption {
              type = types.port;
              default = 3264;
              description = "Port to listen on.";
            };
            addr = mkOption {
              type = types.str;
              default = "0.0.0.0";
              description = "Address to bind to.";
            };
            passFile = mkOption {
              type = types.path;
              description = "File containing the gateway passphrase.";
            };
            fontDirs = mkOption {
              type = types.listOf types.str;
              default = [ ];
              example = [
                "/usr/share/fonts"
                "/home/alice/.local/share/fonts"
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
              default = self.packages.${pkgs.system}.blit;
              defaultText = "self.packages.\${system}.blit";
              description = "The blit package to use for the gateway.";
            };
          };
        }
      );
      default = { };
      description = "Named blit gateway instances connecting to blit server sockets.";
    };

    shares = mkOption {
      type = types.attrsOf (
        types.submodule {
          options = {
            user = mkOption {
              type = types.str;
              description = "User whose blit server socket to share.";
            };
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
              default = self.packages.${pkgs.system}.blit;
              defaultText = "self.packages.\${system}.blit";
              description = "The blit package to use for the share service.";
            };
          };
        }
      );
      default = { };
      description = "Named blit share instances exposing blit server sessions via WebRTC.";
    };
  };

  config = mkIf cfg.enable {
    systemd.services =
      builtins.listToAttrs (
        map (user: {
          name = "blit-server@${user}";
          value = {
            description = "blit terminal multiplexer for ${user}";
            requires = [ "blit-server@${user}.socket" ];
            serviceConfig = {
              Type = "simple";
              User = user;
              WorkingDirectory = "~";
              ExecStart = "${cfg.package}/bin/blit server";
              Environment =
                lib.optional (cfg.shell != null) "SHELL=${cfg.shell}"
                ++ [
                  "BLIT_SCROLLBACK=${toString cfg.scrollback}"
                ]
                ++ lib.optional (gpuLibSearchPath != "") "LD_LIBRARY_PATH=${gpuLibSearchPath}"
                ++ lib.optionals cfg.audio.enable [
                  "BLIT_AUDIO=1"
                  "BLIT_AUDIO_BITRATE=${toString cfg.audio.bitrate}"
                  "PATH=${
                    lib.makeBinPath [
                      pkgs.pipewire
                      pkgs.wireplumber
                      pkgs.dbus
                    ]
                  }"
                ]
                ++ lib.optional (!cfg.audio.enable) "BLIT_AUDIO=0";
            };
          };
        }) cfg.users
      )
      // builtins.listToAttrs (
        lib.mapAttrsToList (name: gw: {
          name = "blit-gateway-${name}";
          value =
            let
              effectiveRemoteFile = gw.remoteFile;
            in
            {
              description = "blit gateway ${name}" + lib.optionalString (gw.user != null) " for ${gw.user}";
              after = lib.optional (gw.user != null) "blit-server@${gw.user}.socket" ++ [ "network.target" ];
              requires = lib.optional (gw.user != null) "blit-server@${gw.user}.socket";
              wantedBy = [ "multi-user.target" ];
              serviceConfig = {
                Type = "simple";
                ExecStart = "${gw.package}/bin/blit gateway";
                Environment = [
                  "BLIT_ADDR=${gw.addr}:${toString gw.port}"
                ]
                ++ lib.optional (effectiveRemoteFile != null) "BLIT_REMOTES=${effectiveRemoteFile}"
                ++ lib.optional (gw.fontDirs != [ ]) "BLIT_FONT_DIRS=${lib.concatStringsSep ":" gw.fontDirs}"
                ++ lib.optional gw.storeConfig "BLIT_STORE_CONFIG=1"
                ++ lib.optional gw.quic "BLIT_QUIC=1"
                ++ lib.optional (gw.tlsCert != null) "BLIT_TLS_CERT=${gw.tlsCert}"
                ++ lib.optional (gw.tlsKey != null) "BLIT_TLS_KEY=${gw.tlsKey}"
                ++ lib.optional gw.webrtcProxy "BLIT_GATEWAY_WEBRTC=1"
                ++ lib.optional (gw.hub != null) "BLIT_HUB=${gw.hub}";
                EnvironmentFile = gw.passFile;
              }
              // lib.optionalAttrs (gw.user != null) {
                User = gw.user;
              }
              // lib.optionalAttrs (gw.port < 1024) {
                AmbientCapabilities = [ "CAP_NET_BIND_SERVICE" ];
              };
            };
        }) cfg.gateways
      )
      // builtins.listToAttrs (
        lib.mapAttrsToList (name: shr: {
          name = "blit-share-${name}";
          value = {
            description = "blit share ${name} for ${shr.user}";
            after = [
              "blit-server@${shr.user}.socket"
              "network.target"
            ];
            requires = [ "blit-server@${shr.user}.socket" ];
            wantedBy = [ "multi-user.target" ];
            serviceConfig = {
              Type = "simple";
              User = shr.user;
              ExecStart =
                "${shr.package}/bin/blit share"
                + lib.optionalString shr.quiet " --quiet"
                + lib.optionalString shr.verbose " --verbose";
              Environment = [
                "BLIT_SOCK=/run/blit/${shr.user}.sock"
              ]
              ++ lib.optional (shr.hub != null) "BLIT_HUB=${shr.hub}"
              ++ lib.optional shr.verboseWebrtc "BLIT_WEBRTC_VERBOSE=1";
              EnvironmentFile = shr.passFile;
              Restart = "on-failure";
            };
          };
        }) cfg.shares
      );

    systemd.sockets = builtins.listToAttrs (
      map (user: {
        name = "blit-server@${user}";
        value = {
          description = "blit terminal multiplexer socket for ${user}";
          wantedBy = [ "sockets.target" ];
          socketConfig = {
            ListenStream = "/run/blit/${user}.sock";
            SocketUser = user;
            SocketMode = "0700";
            RuntimeDirectory = "blit";
            RuntimeDirectoryMode = "0755";
          };
        };
      }) cfg.users
    );
  };
}
