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

  # blit server loads GPU codec libraries via dlopen at runtime:
  #   VA-API:  libva.so.2, libva-drm.so.2  (from pkgs.libva)
  #   NVDEC:   libcuda.so.1, libnvcuvid.so.1         (from the GPU driver)
  #   NVENC:   libcuda.so.1, libnvidia-encode.so.1   (from the GPU driver)
  # On NixOS these live under /nix/store and are not in the default
  # ld.so search path.  addDriverRunpath.driverLink is the NixOS-managed
  # symlink farm (/run/opengl-driver) for the active GPU driver (NVIDIA,
  # Mesa, etc.) and covers NVENC, CUDA, and VA-API backend drivers.
  gpuLibSearchPath = lib.makeLibraryPath (cfg.gpuLibraries ++ [ pkgs.addDriverRunpath.driverLink ]);

  # The server also dlopens libpipewire-0.3.so.0 directly when audio is
  # enabled (replacing the former pw-cat subprocess).  Add pipewire's
  # library dir to the loader path so the dlopen resolves.
  audioLibSearchPath = lib.makeLibraryPath [ pkgs.pipewire ];

  # Combined LD_LIBRARY_PATH for the server unit. GPU and audio paths remain
  # conditional; software camera decoders are compiled into blit.
  serverLibSearchPath = lib.concatStringsSep ":" (
    lib.optional (gpuLibSearchPath != "") gpuLibSearchPath
    ++ lib.optional cfg.audio.enable audioLibSearchPath
  );

  # Resolve the user's normal Nix profiles once for both PATH and
  # `XDG_DATA_DIRS`. The latter is where the list of installed applications
  # comes from: the `session` extension asks the server for its environment
  # (`ENV_GET`) and scans `$XDG_DATA_DIRS/*/applications` for `.desktop` files
  # (extensions/session/src/main.rs). A unit inherits none of the login
  # environment, so without this the extension falls back to the spec's default
  # of `/usr/local/share:/usr/share` — neither of which exists on NixOS — and the
  # only applications anyone can launch are whatever happens to be under
  # `~/.local/share/applications`. Everything installed through a Nix profile is
  # invisible.
  #
  # `environment.profiles` is the same list `/etc/profile` turns into
  # `XDG_DATA_DIRS` for an interactive shell, so a session sees the applications
  # its user would see on the console, in the same precedence order, including
  # whatever other modules (Flatpak) have added.
  userProfileRoots =
    user:
    let
      home = lib.attrByPath [ user "home" ] "/home/${user}" config.users.users;
      # systemd does no shell expansion in `Environment=`, so a profile written
      # in terms of another variable would land as a literal and resolve to
      # nothing. Drop those rather than ship a root that cannot exist.
      resolvable = lib.filter (profile: !(lib.hasInfix "\${" profile)) config.environment.profiles;
    in
    map (profile: lib.replaceStrings [ "$HOME" "$USER" ] [ home user ] profile) resolvable;

  userDataDirs =
    user: lib.concatMapStringsSep ":" (profile: profile + "/share") (userProfileRoots user);
in
{
  options.services.blit = {
    enable = mkEnableOption "blit terminal multiplexer";

    package = mkOption {
      type = types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.blit;
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
        blit server instance at /run/blit/<user>-default.sock.
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

    languageServers = mkOption {
      type = types.listOf types.package;
      default = [ ];
      example = lib.literalExpression "[ pkgs.nixd pkgs.rust-analyzer pkgs.gopls ]";
      description = ''
        Language servers to place on the blit server's PATH so
        <literal>blit lsp</literal> (docs/design/lsp.md) can discover and
        spawn them. blit ships none; list the servers you want available
        and their binaries are added to the server process's PATH. blit
        matches them to files by project marker and extension, keeps them
        warm across connections, and never downloads anything. Empty by
        default (the family is advertised but finds no servers). Set
        <option>BLIT_LSP=0</option> via the environment to disable the
        family entirely.
      '';
    };

    extensions = {
      persistent = mkOption {
        type = types.bool;
        default = true;
        description = ''
          Permit durable Wasm and JavaScript extensions (<literal>blit ext run
          --persist</literal>) and start the ones that should be running again
          after a restart. This is also what makes an extension's
          <literal>@name</literal> command namespace exist. Setting it false
          passes <option>BLIT_ALLOW_EXT_PERSIST=0</option>, which is the
          recovery path for a persistent definition that breaks the server it
          starts in; transient extensions still run without it.

          Definitions live in
          <filename>~/.local/state/blit/instances/default/extensions.redb</filename>
          and module bytes in
          <filename>~/.cache/blit/instances/default/wasm</filename>, so
          clearing the cache blocks every persistent extension until one is
          uploaded again.
        '';
      };

      path = mkOption {
        type = types.listOf types.package;
        default = [ ];
        example = lib.literalExpression "[ pkgs.glib.bin ]";
        description = ''
          Extra packages on the blit server's PATH, for the processes
          extensions spawn. An extension reaches the machine only through
          protocol operations such as starting a child process, and a server
          started from this unit has little more than coreutils and systemd on
          its PATH — so whatever an extension shells out to belongs here.

          <literal>pkgs.glib.bin</literal> supplies <literal>gdbus</literal>,
          which lets the systemd extension react to unit changes as they
          happen instead of polling for them.
        '';
      };
    };

    x11 = {
      enable = mkOption {
        type = types.bool;
        default = true;
        description = ''
          Run X11 applications in blit sessions, through
          <literal>xwayland-satellite</literal>. blit's compositor speaks
          Wayland only, so without a bridge an X11-only application does not
          fall back — it fails to start, and a toolkit that can do both is
          told to use Wayland.

          The bridge is started per session only when its binary is on the
          server's PATH, which is what this option arranges; the server
          itself never requires it. Turning this off (or setting
          <option>BLIT_XWAYLAND=0</option>) keeps Xwayland out of the
          closure and leaves sessions Wayland-only.
        '';
      };

      package = mkOption {
        type = types.package;
        default = pkgs.xwayland-satellite;
        defaultText = "pkgs.xwayland-satellite";
        description = "The X11 bridge to put on the server's PATH.";
      };
    };

    audio = {
      enable = mkEnableOption "audio forwarding (PipeWire capture + Opus)";

      bitrate = mkOption {
        type = types.int;
        default = 64000;
        description = "Opus encoder bitrate in bits/sec.";
      };

      realtime = mkOption {
        type = types.bool;
        default = true;
        description = ''
          Enable RTKit, so blit's private PipeWire graph can run its data
          loop at realtime priority.

          PipeWire asks for <literal>SCHED_FIFO</literal> and carries on
          without it, silently, so the failure looks like nothing at all
          until the machine is busy. A desktop session normally supplies
          the privilege through RTKit; a server started from a socket unit
          or a development shell has no session, so the audio loop ends up
          on <literal>SCHED_OTHER</literal> at priority 0 and competes with
          the compositor and the video encoders it shares a machine with.
          It then misses its cycle deadline exactly when there is most to
          do — scrolling a window, resizing, anything that saturates a core
          — and the gap is cut into the captured audio itself, before any
          of it is encoded or sent. No client-side jitter buffer can
          recover audio that was never captured.

          RTKit rather than a raised <literal>rtprio</literal> limit on the
          unit: rlimits are inherited, and the server spawns the user's
          shells, so a limit here would hand every process started from a
          terminal the same ceiling. RTKit grants the priority per thread,
          to the process that asks, and polices what it hands out — and
          being a system service it also covers a server run by hand
          outside systemd, which an rlimit on the unit would not.

          Sets <option>security.rtkit.enable</option>; turn this off if you
          manage realtime privileges yourself.
        '';
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
        for hardware-accelerated video decoding/encoding and GPU compositing.
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
              description = "File containing BLIT_PASSPHRASE=<passphrase> or BLIT_PASSPHRASE=<argon2 PHC hash>.";
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
              default = self.packages.${pkgs.stdenv.hostPlatform.system}.blit;
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
              default = self.packages.${pkgs.stdenv.hostPlatform.system}.blit;
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
    # PipeWire's data loop is worthless at SCHED_OTHER on a machine that also
    # encodes video: it misses its cycle and the gap lands in the captured
    # audio. `mkDefault` so a host that manages realtime privileges its own
    # way keeps the last word.
    security.rtkit.enable = lib.mkIf (cfg.audio.enable && cfg.audio.realtime) (
      lib.mkDefault true
    );

    systemd.services =
      builtins.listToAttrs (
        map (user: {
          name = "blit-server@${user}";
          value = {
            description = "blit terminal multiplexer for ${user}";
            requires = [ "blit-server@${user}.socket" ];
            # Audio spawns pipewire / wireplumber / dbus-daemon by name,
            # so they need to be on $PATH.  Language servers likewise are
            # spawned by name and discovered via PATH (docs/design/lsp.md).
            # Use systemd.services.*.path (which prepends to the default
            # PATH) rather than overriding $PATH in Environment — that
            # would clobber coreutils and friends for PTY shells, which
            # inherit the service env.
            path =
              lib.optionals cfg.audio.enable [
                pkgs.pipewire
                pkgs.wireplumber
                pkgs.dbus
              ]
              # The portal frontend is spawned by name for blit's private bus
              # (crates/server/src/desktop_bus.rs), and it is unconditional:
              # the camera and ScreenCast portals are how Firefox and
              # Chromium find a viewer's camera and answer getDisplayMedia,
              # neither of which has anything to do with audio forwarding.
              # This package ships no bin/, so the libexec directory has to
              # go on PATH directly — listing the package would add an empty
              # bin/ and the frontend would silently never start.
              ++ lib.optional pkgs.stdenv.isLinux "${pkgs.xdg-desktop-portal}/libexec"
              # Spawned by name, once per session, and only if it is here:
              # see crates/server/src/xwayland.rs.
              ++ lib.optional (pkgs.stdenv.isLinux && cfg.x11.enable) cfg.x11.package
              ++ cfg.languageServers
              # Whatever the extensions on this server shell out to.
              ++ cfg.extensions.path
              # A socket-activated system service does not source /etc/profile,
              # but exact-argv terminals and extensions still need the user's
              # normal Nix profiles. In particular, muster can find `direnv`
              # in the per-user profile and `nix` in the default profile before
              # entering a checkout's flake environment.
              ++ userProfileRoots user;
            serviceConfig = {
              Type = "notify";
              User = user;
              WorkingDirectory = "~";
              ExecStart = "${cfg.package}/bin/blit server";
              # Let PipeWire's module-rt put the graph thread on SCHED_FIFO.
              #
              # The audio graph runs a 21 ms cycle and shares this host with
              # video encoding. Without an RT budget module-rt cannot raise
              # the thread and falls back to nice — which RLIMIT_NICE of 0
              # also refuses — so `data-loop.0` runs SCHED_OTHER at nice 0
              # against the encoder. It then misses cycles under load and
              # emits audio in bursts: measured 60-110 ms holes with nothing
              # else on the wire, which no jitter buffer sized for a 20 ms
              # cadence can absorb. RTKit is the other route and does not
              # reach this graph, whose PipeWire runs a stripped config.
              LimitRTPRIO = 95;
              LimitNICE = "-11";
              Environment =
                lib.optional (cfg.shell != null) "SHELL=${cfg.shell}"
                ++ [
                  "BLIT_SCROLLBACK=${toString cfg.scrollback}"
                ]
                ++ lib.optional (userDataDirs user != "") "XDG_DATA_DIRS=${userDataDirs user}"
                ++ lib.optional (serverLibSearchPath != "") "LD_LIBRARY_PATH=${serverLibSearchPath}"
                ++ lib.optionals cfg.audio.enable [
                  "BLIT_AUDIO=1"
                  "BLIT_AUDIO_BITRATE=${toString cfg.audio.bitrate}"
                ]
                ++ lib.optional (!cfg.audio.enable) "BLIT_AUDIO=0"
                ++ lib.optional (!cfg.extensions.persistent) "BLIT_ALLOW_EXT_PERSIST=0";
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
                Type = "notify";
                ExecStart = "${gw.package}/bin/blit gateway";
                Environment = [
                  "BLIT_ADDR=${gw.addr}:${toString gw.port}"
                ]
                ++ lib.optional (gw.user != null) "BLIT_SOCK=/run/blit/${gw.user}-default.sock"
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
              Type = "notify";
              User = shr.user;
              ExecStart =
                "${shr.package}/bin/blit share"
                + lib.optionalString shr.quiet " --quiet"
                + lib.optionalString shr.verbose " --verbose";
              Environment = [
                "BLIT_SOCK=/run/blit/${shr.user}-default.sock"
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
            ListenStream = "/run/blit/${user}-default.sock";
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
