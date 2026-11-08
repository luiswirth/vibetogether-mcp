{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

    rust-overlay.url = "github:oxalica/rust-overlay";
    rust-overlay.inputs.nixpkgs.follows = "nixpkgs";

    treefmt-nix.url = "github:numtide/treefmt-nix";
    treefmt-nix.inputs.nixpkgs.follows = "nixpkgs";
  };

  outputs = {
    nixpkgs,
    rust-overlay,
    treefmt-nix,
    ...
  }: let
    forEachSystem = f:
      nixpkgs.lib.genAttrs ["aarch64-darwin" "x86_64-linux"]
      (system:
        f (import nixpkgs {
          inherit system;
          overlays = [(import rust-overlay)];
        }));

    # One environment, entered by the shell and by the checks alike.
    tools = pkgs:
      with pkgs; [
        (rust-bin.fromRustupToolchainFile ./rust-toolchain.toml)
        bacon
        cargo-edit
        just
      ];
  in {
    # Every language in the tree, behind one command. The global pre-commit
    # hook checks a repository by running this, so a language left out here is
    # a language nothing checks.
    formatter = forEachSystem (pkgs:
      treefmt-nix.lib.mkWrapper pkgs {
        projectRootFile = "flake.nix";
        programs.rustfmt.enable = true;
        programs.alejandra.enable = true;
        programs.just.enable = true;
      });

    devShells = forEachSystem (pkgs: {
      default = pkgs.mkShell {packages = tools pkgs;};
    });
  };
}
