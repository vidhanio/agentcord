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
            cfg = config.programs.agentcord;
            toml = pkgs.formats.toml { };
          in
          {
            options.programs.agentcord = {
              enable = lib.mkEnableOption "agentcord";

              package = lib.mkOption {
                type = lib.types.package;
                default = self.packages.${pkgs.system}.default;
                description = "Package to install for Agentcord.";
              };

              settings = lib.mkOption {
                inherit (toml) type;
                default = { };
                description = "Configuration written to `agentcord/config.toml`.";
              };

              environmentFile = lib.mkOption {
                type = lib.types.nullOr lib.types.path;
                default = null;
                example = "/run/secrets/agentcord";
                description = ''
                  Path to a file containing environment variables for the agentcord
                  service, in the format of an EnvironmentFile as described by
                  {manpage}`systemd.exec(5)` (i.e. `KEY=VALUE` pairs, one per line).

                  This can be used to keep secrets such as the Discord bot token
                  out of the Nix store. Reference them from {option}`settings` using
                  `''${NAME}` placeholders.
                '';
              };
            };

            config = lib.mkIf cfg.enable {
              home.packages = [ cfg.package ];
              xdg.configFile."agentcord/config.toml".source = toml.generate "agentcord-config.toml" cfg.settings;

              systemd.user.services.agentcord = {
                Unit = {
                  Description = "Agentcord Discord client";
                  After = [ "network.target" ];
                };

                Service = {
                  ExecStart = lib.getExe cfg.package;
                  Restart = "always";
                  RestartSec = 5;
                }
                // lib.optionalAttrs ((cfg.environmentFile or null) != null) {
                  EnvironmentFile = cfg.environmentFile;
                };

                Install = {
                  WantedBy = [ "default.target" ];
                };
              };
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
            # `tests` directory. Anything else (docs, CI, dotfiles) is noise
            # that would churn rebuilds.
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
                  meta.mainProgram = "agentcord";
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

              # Fetch git dependencies (poise/serenity) with the system `git`,
              # which honors the machine's GitHub SSH configuration, instead
              # of libgit2, which fails on SSH host-key validation.
              env.CARGO_NET_GIT_FETCH_WITH_CLI = "true";

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
