# Nix modules

## nix-darwin

```nix
{ inputs, ... }: {
  imports = [ inputs.blit.darwinModules.blit ];

  services.blit = {
    enable = true;
    gateways.default = {
      port = 3264;
      passFile = "/path/to/blit-pass-env";
      # Optional: proxy share: remotes via WebRTC (BLIT_GATEWAY_WEBRTC=1)
      webrtcProxy = true;
      hub = "hub.blit.sh"; # default hub for share: remotes
    };
    shares.default = {
      passFile = "/path/to/blit-share-env";
    };
  };
}
```

See [`darwin-module.nix`](darwin-module.nix) for the full list of options.

### Gateway remotes (nix-darwin)

By default the gateway reads `~/.config/blit/blit.remotes`, the same file managed by `blit remote add`. Remotes persist across gateway restarts. To use a declarative file instead, set `remoteFile`:

```nix
gateways.default = {
  port = 3264;
  passFile = "/path/to/blit-pass-env";
  remoteFile = "/path/to/blit.remotes"; # optional: overrides ~/.config/blit/blit.remotes
};
```

## NixOS

```nix
{ inputs, ... }: {
  imports = [ inputs.blit.nixosModules.blit ];

  services.blit = {
    enable = true;
    users = [ "alice" "bob" ];
    gateways.alice = {
      user = "alice";
      port = 3264;
      passFile = "/run/secrets/blit-alice-pass";
      # Optional: proxy share: remotes via WebRTC (BLIT_GATEWAY_WEBRTC=1)
      webrtcProxy = true;
    };
    shares.alice = {
      user = "alice";
      passFile = "/run/secrets/blit-alice-share-pass";
    };
  };
}
```

See [`nixos-module.nix`](nixos-module.nix) for the full list of options.

### Gateway remotes (NixOS)

By default the gateway reads `~/.config/blit/blit.remotes`, the same file managed by `blit remote add`. Remotes persist across gateway restarts. To use a declarative file instead, set `remoteFile`:

```nix
gateways.alice = {
  user = "alice";
  port = 3264;
  passFile = "/run/secrets/blit-alice-pass";
  remoteFile = "/etc/blit/alice.remotes"; # optional: overrides ~/.config/blit/blit.remotes
};
```

### Gateway `webrtcProxy` option

When `webrtcProxy = true`, the gateway sets `BLIT_GATEWAY_WEBRTC=1` and connects to `share:` entries in `blit.remotes` as a WebRTC consumer, re-exposing them over its normal WebSocket/WebTransport path. Browsers see the destination as a regular `WS /d/<name>` connection and do not need direct access to the signaling hub.

The optional `hub` option sets `BLIT_HUB` (default: `hub.blit.sh`) and can be overridden per-remote in `blit.remotes` with `name = share:passphrase?hub=wss://custom.hub`.
