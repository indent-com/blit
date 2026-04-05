{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay.url = "github:oxalica/rust-overlay";
    crane.url = "github:ipetkov/crane";
    flake-parts.url = "github:hercules-ci/flake-parts";
  };

  outputs =
    inputs:
    inputs.flake-parts.lib.mkFlake { inherit inputs; } {
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "x86_64-darwin"
        "aarch64-darwin"
      ];

      imports = [
        ./nix/packages.nix
      ];

      flake = {
        darwinModules.default = inputs.self.darwinModules.blit;
        darwinModules.blit = import ./nix/darwin-module.nix inputs.self;

        nixosModules.default = inputs.self.nixosModules.blit;
        nixosModules.blit = import ./nix/nixos-module.nix inputs.self;
      };
    };
}
