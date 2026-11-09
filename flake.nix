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
          pname = "vibetogether-mcp";
          version = "0.1.0";
          src = pkgs.lib.fileset.toSource {
            root = ./.;
            fileset = pkgs.lib.fileset.unions [./Cargo.toml ./Cargo.lock ./src ./tests];
          };
          cargoLock = {
            lockFile = ./Cargo.lock;
            # The participant library, and the relay the tests run: git
            # dependencies on one commit of agentwire.
            outputHashes."agentwire-0.1.0" = "sha256-sfxPuXbcuFzDhyXB0WfNPQLY65ZsheOV8ZjcyKaru0s=";
            outputHashes."agentwire-relay-0.1.0" = "sha256-sfxPuXbcuFzDhyXB0WfNPQLY65ZsheOV8ZjcyKaru0s=";
          };
          # The binary names the toolchain's sources, in panic messages, which
          # would pull all of it into whatever runs the server.
          nativeBuildInputs = [pkgs.removeReferencesTo];
          # The HTTP client loads the system's certificates even for the tests'
          # local relay, and the build sandbox has none.
          nativeCheckInputs = [pkgs.cacert];
          postFixup = "remove-references-to -t ${rust pkgs} $out/bin/vibetogether-mcp";
          disallowedReferences = [(rust pkgs)];
          meta.mainProgram = "vibetogether-mcp";
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
