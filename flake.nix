{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    flake-parts.url = "github:hercules-ci/flake-parts";
    crane.url = "github:ipetkov/crane";
    systems.url = "github:nix-systems/default-linux";
    treefmt-nix.url = "github:numtide/treefmt-nix";
    rust-overlay.url = "github:oxalica/rust-overlay";
  };

  outputs =
    inputs:
    inputs.flake-parts.lib.mkFlake { inherit inputs; } (
      {
        inputs,
        self,
        ...
      }:
      {
        imports = [ inputs.treefmt-nix.flakeModule ];

        systems = import inputs.systems;

        flake.homeManagerModules.default =
          {
            config,
            lib,
            pkgs,
            ...
          }:
          let
            cfg = config.programs.herdcord;
            toml = pkgs.formats.toml { };
          in
          {
            options.programs.herdcord = {
              enable = lib.mkEnableOption "herdcord";

              package = lib.mkOption {
                type = lib.types.package;
                default = self.packages.${pkgs.system}.default;
                description = "Package to install for herdcord.";
              };

              settings = lib.mkOption {
                inherit (toml) type;
                default = { };
                description = "Configuration written to `herdcord/config.toml`.";
              };
            };

            config = lib.mkIf cfg.enable {
              home.packages = [ cfg.package ];
              xdg.configFile."herdcord/config.toml".source = toml.generate "herdcord-config.toml" cfg.settings;
            };
          };

        perSystem =
          {
            system,
            pkgs,
            self',
            config,
            ...
          }:
          let
            craneLib = (inputs.crane.mkLib pkgs).overrideToolchain (
              p:
              p.rust-bin.nightly.latest.default.override {
                extensions = [
                  "rust-src"
                  "rust-analyzer"
                ];
              }
            );

            # Only cargo sources reach the derivation: crane's standard
            # filter (directories, `.rs`, `.toml`, `Cargo.lock`) plus the
            # whole `tests` directory (fixtures with captured herdr API JSON
            # are embedded via `include_str!`). Anything else (docs, CI,
            # dotfiles) is noise that would churn rebuilds.
            src = pkgs.lib.cleanSourceWith {
              src = ./.;
              filter =
                path: type:
                craneLib.filterCargoSources path type || pkgs.lib.hasPrefix (toString ./tests) (toString path);
            };
            commonArgs = {
              inherit src;
              strictDeps = true;
            };

            mkCargoTool =
              {
                pname,
                version,
                src,
                cargoLock,
                ...
              }@args:
              craneLib.buildPackage (
                {
                  inherit
                    pname
                    version
                    src
                    cargoLock
                    ;
                  cargoArtifacts = craneLib.buildDepsOnly {
                    inherit
                      pname
                      version
                      src
                      cargoLock
                      ;
                    strictDeps = true;
                    doCheck = false;
                  };
                  cargoExtraArgs = "--locked";
                  doCheck = false;
                  strictDeps = true;
                }
                // args
              );

            cargoDocsRs = mkCargoTool {
              pname = "cargo-docs-rs";
              version = "1.0.4";
              src = pkgs.fetchFromGitHub {
                owner = "dtolnay";
                repo = "cargo-docs-rs";
                rev = "cd8275c03281264975ca5ea68373ba487d2dcea3";
                hash = "sha256-969GTfOnPUQlDEqupIaP7dX3zexJr2+j+e/nuAgJu1o=";
              };
              cargoLock = ./nix/cargo-docs-rs.Cargo.lock;
            };

            cargoEdit = pkgs.cargo-edit;
            cargoArtifacts = craneLib.buildDepsOnly commonArgs;
          in
          {
            _module.args.pkgs = import inputs.nixpkgs {
              inherit system;
              overlays = [ inputs.rust-overlay.overlays.default ];
            };

            packages = {
              default = craneLib.buildPackage (
                commonArgs
                // {
                  inherit cargoArtifacts;
                }
              );

              cargo-docs-rs = cargoDocsRs;
              cargo-edit = cargoEdit;
            };

            checks = {
              clippy = craneLib.cargoClippy (
                commonArgs
                // {
                  inherit cargoArtifacts;
                  cargoClippyExtraArgs = "--all-targets -- --deny warnings";
                }
              );

              doc = craneLib.mkCargoDerivation (
                commonArgs
                // {
                  inherit cargoArtifacts;
                  nativeBuildInputs = [ cargoDocsRs ];
                  buildPhaseCargoCommand = "cargo docs-rs --locked";
                  installPhaseCommand = "mkdir -p $out";
                  env.RUSTDOCFLAGS = "--deny warnings";
                  doCheck = false;
                }
              );

              fmt = craneLib.cargoFmt {
                inherit src;
              };

              deny = craneLib.cargoDeny {
                inherit src;
              };

              nextest = craneLib.cargoNextest (
                commonArgs
                // {
                  inherit cargoArtifacts;
                  partitions = 1;
                  partitionType = "count";
                  cargoNextestPartitionsExtraArgs = "--no-tests=pass";
                }
              );
            };

            devShells.default = craneLib.devShell {
              inherit (self') checks;

              packages = [
                cargoDocsRs
                cargoEdit
                pkgs.nil
                pkgs.prek
                config.treefmt.build.wrapper
              ];
            };

            treefmt = {
              programs = {
                nixfmt.enable = true;
                statix.enable = true;
                deadnix.enable = true;
                rustfmt = {
                  enable = true;
                  package = pkgs.rust-bin.nightly.latest.rustfmt;
                };
                taplo.enable = true;
              };
            };
          };
      }
    );
}
