{
  description = "kylecord — a Discord client";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-parts.url = "github:hercules-ci/flake-parts";
    systems.url = "github:nix-systems/default";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    crane.url = "github:ipetkov/crane";
  };

  outputs =
    inputs@{ flake-parts, ... }:
    flake-parts.lib.mkFlake { inherit inputs; } {
      systems = import inputs.systems;

      perSystem =
        {
          system,
          lib,
          ...
        }:
        let
          pkgs = import inputs.nixpkgs {
            inherit system;
            overlays = [ (import inputs.rust-overlay) ];
          };

          rustToolchain = pkgs.rust-bin.stable.latest.default.override {
            extensions = [
              "rust-src"
              "rust-analyzer"
              "clippy"
              "rustfmt"
            ];
          };

          craneLib = (inputs.crane.mkLib pkgs).overrideToolchain rustToolchain;
          src = craneLib.cleanCargoSource ./.;

          gstPlugins = with pkgs.gst_all_1; [
            gstreamer
            gst-plugins-base
            gst-plugins-good
            gst-plugins-bad
            gst-plugins-ugly
            gst-libav
            gst-plugins-rs
          ];

          nativeBuildInputs = with pkgs; [
            clang
            pkg-config
            wrapGAppsHook4
          ];

          buildInputs = with pkgs; [
            gtk4
            libadwaita
            opus
            pipewire
          ] ++ gstPlugins;

          commonArgs = {
            inherit src nativeBuildInputs buildInputs;
            pname = "kylecord";
            strictDeps = true;
            GIT_HASH = inputs.self.shortRev or "dirty";
            GST_PLUGIN_SYSTEM_PATH_1_0 = lib.makeSearchPath "lib/gstreamer-1.0" gstPlugins;
            LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
          };

          cargoArtifacts = craneLib.buildDepsOnly commonArgs;

          kylecord = craneLib.buildPackage (
            commonArgs
            // {
              inherit cargoArtifacts;
              pname = "kylecord";
              version = "0.1.0";
              gappsWrapperArgs = [
                "--prefix"
                "GST_PLUGIN_SYSTEM_PATH_1_0"
                ":"
                (lib.makeSearchPath "lib/gstreamer-1.0" gstPlugins)
              ];
              meta = {
                description = "A Discord client";
                license = lib.licenses.gpl3Only;
                platforms = lib.platforms.linux;
                mainProgram = "kylecord";
              };
            }
          );
        in
        {
          packages.default = kylecord;

          devShells.default = pkgs.mkShell {
            inputsFrom = [ kylecord ];
            packages = [
              rustToolchain
              pkgs.cargo-nextest
              pkgs.grim
              pkgs.nixfmt
            ];
            shellHook = ''
              export RUST_BACKTRACE=1
              export XDG_DATA_DIRS="${pkgs.libadwaita}/share:${pkgs.gtk4}/share:${pkgs.gsettings-desktop-schemas}/share:$XDG_DATA_DIRS"
              export GST_PLUGIN_SYSTEM_PATH_1_0="${lib.makeSearchPath "lib/gstreamer-1.0" gstPlugins}:$GST_PLUGIN_SYSTEM_PATH_1_0"
              export LIBCLANG_PATH="${pkgs.llvmPackages.libclang.lib}/lib"
            '';
          };

          formatter = pkgs.nixfmt;

          checks = {
            inherit kylecord;
            clippy = craneLib.cargoClippy (
              commonArgs
              // {
                inherit cargoArtifacts;
                cargoClippyExtraArgs = "--workspace --all-targets -- -D warnings";
              }
            );
            fmt = craneLib.cargoFmt { inherit src; };
            tests = craneLib.cargoTest (commonArgs // { inherit cargoArtifacts; });
          };
        };
    };
}