self: { config, lib, pkgs, ... }:
let
  cfg = config.services.blit;
  inherit (lib) mkEnableOption mkOption types mkIf;
in {
  options.services.blit = {
    enable = mkEnableOption "blit terminal multiplexer";

    package = mkOption {
      type = types.package;
      default = self.packages.${pkgs.system}.blit-server;
      defaultText = "self.packages.\${system}.blit-server";
      description = "The blit-server package to use.";
    };

    users = mkOption {
      type = types.listOf types.str;
      default = [];
      example = [ "alice" "bob" ];
      description = ''
        Users to enable blit for. Each user gets a socket-activated
        blit-server instance at /run/blit/<user>.sock.
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

    gateways = mkOption {
      type = types.attrsOf (types.submodule {
        options = {
          user = mkOption {
            type = types.str;
            description = "User whose blit-server socket to connect to.";
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
            default = [];
            example = [ "/usr/share/fonts" "/home/alice/.local/share/fonts" ];
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
          storeConfig = mkOption {
            type = types.bool;
            default = false;
            description = "Sync browser settings to ~/.config/blit/blit.conf.";
          };
          package = mkOption {
            type = types.package;
            default = self.packages.${pkgs.system}.blit-gateway;
            defaultText = "self.packages.\${system}.blit-gateway";
            description = "The blit-gateway package to use.";
          };
        };
      });
      default = {};
      description = "Named blit-gateway instances connecting to blit-server sockets.";
    };
  };

  config = mkIf cfg.enable {
    systemd.services = builtins.listToAttrs (map (user: {
      name = "blit@${user}";
      value = {
        description = "blit terminal multiplexer for ${user}";
        requires = [ "blit@${user}.socket" ];
        serviceConfig = {
          Type = "simple";
          User = user;
          WorkingDirectory = "~";
          ExecStart = let
            serverBin = "${cfg.package}/bin/blit-server";
          in "${serverBin}";
          Environment = lib.optional (cfg.shell != null) "SHELL=${cfg.shell}"
            ++ [ "BLIT_SCROLLBACK=${toString cfg.scrollback}" ];
        };
      };
    }) cfg.users)
    // builtins.listToAttrs (lib.mapAttrsToList (name: gw: {
      name = "blit-gateway-${name}";
      value = {
        description = "blit gateway ${name} for ${gw.user}";
        after = [ "blit@${gw.user}.socket" "network.target" ];
        requires = [ "blit@${gw.user}.socket" ];
        wantedBy = [ "multi-user.target" ];
        serviceConfig = {
          Type = "simple";
          User = gw.user;
          ExecStart = "${gw.package}/bin/blit-gateway";
          Environment = [
            "BLIT_SOCK=/run/blit/${gw.user}.sock"
            "BLIT_ADDR=${gw.addr}:${toString gw.port}"
          ] ++ lib.optional (gw.fontDirs != []) "BLIT_FONT_DIRS=${lib.concatStringsSep ":" gw.fontDirs}"
            ++ lib.optional gw.storeConfig "BLIT_STORE_CONFIG=1"
            ++ lib.optional gw.quic "BLIT_QUIC=1"
            ++ lib.optional (gw.tlsCert != null) "BLIT_TLS_CERT=${gw.tlsCert}"
            ++ lib.optional (gw.tlsKey != null) "BLIT_TLS_KEY=${gw.tlsKey}";
          EnvironmentFile = gw.passFile;
          AmbientCapabilities = lib.mkIf (gw.port < 1024) [ "CAP_NET_BIND_SERVICE" ];
        };
      };
    }) cfg.gateways);

    systemd.sockets = builtins.listToAttrs (map (user: {
      name = "blit@${user}";
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
    }) cfg.users);
  };
}
