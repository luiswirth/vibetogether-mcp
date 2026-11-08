{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

    rust-overlay.url = "github:oxalica/rust-overlay";
    rust-overlay.inputs.nixpkgs.follows = "nixpkgs";

    treefmt-nix.url = "github:numtide/treefmt-nix";
    treefmt-nix.inputs.nixpkgs.follows = "nixpkgs";
  };

  outputs = {
    self,
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

    # The one toolchain, which the shell and the package share.
    rust = pkgs: pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;

    # One environment, entered by the shell and by the checks alike.
    tools = pkgs:
      with pkgs; [
        (rust pkgs)
        bacon
        cargo-edit
        just
      ];
  in {
    packages = forEachSystem (pkgs: {
      default =
        (pkgs.makeRustPlatform {
          cargo = rust pkgs;
          rustc = rust pkgs;
        }).buildRustPackage {
          pname = "vibe-together-mcp";
          version = "0.1.0";
          src = pkgs.lib.fileset.toSource {
            root = ./.;
            fileset = pkgs.lib.fileset.unions [./Cargo.toml ./Cargo.lock ./src ./tests];
          };
          cargoLock = {
            lockFile = ./Cargo.lock;
            # The relay the tests run, a git dependency.
            outputHashes."agentwire-relay-0.1.0" = "sha256-7DygMywoJV/5obTwYCViV4fR4yHoyBw73n3+n2jK0mM=";
          };
          # The binary names the toolchain's sources, in panic messages, which
          # would pull all of it into whatever runs the server.
          nativeBuildInputs = [pkgs.removeReferencesTo];
          # The HTTP client loads the system's certificates even for the tests'
          # local relay, and the build sandbox has none.
          nativeCheckInputs = [pkgs.cacert];
          postFixup = "remove-references-to -t ${rust pkgs} $out/bin/vibe-together-mcp";
          disallowedReferences = [(rust pkgs)];
          meta.mainProgram = "vibe-together-mcp";
        };
    });

    nixosModules.default = import ./nixos-module.nix self;

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
