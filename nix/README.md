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
    };
    forwarders.default = {
      passFile = "/path/to/blit-forwarder-env";
    };
  };
}
```

See [`darwin-module.nix`](darwin-module.nix) for the full list of options.

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
    };
    forwarders.alice = {
      user = "alice";
      passFile = "/run/secrets/blit-alice-forwarder-pass";
    };
  };
}
```

See [`nixos-module.nix`](nixos-module.nix) for the full list of options.
