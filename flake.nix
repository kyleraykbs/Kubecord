{
  description = "kubecord — a Discord client";

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
          # Cargo sources plus the bundled asset files (e.g. the spell-check
          # word list), which cleanCargoSource would otherwise drop.
          src = lib.cleanSourceWith {
            src = ./.;
            filter =
              path: type:
              (builtins.match ".*/assets/.*" path != null) || (craneLib.filterCargoSources path type);
          };

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
            # The in-app captcha view (the `captcha` feature); WebKit needs
            # glib-networking for TLS.
            webkitgtk_6_0
            glib-networking
          ] ++ gstPlugins;

          commonArgs = {
            inherit src nativeBuildInputs buildInputs;
            pname = "kubecord";
            strictDeps = true;
            GIT_HASH = inputs.self.shortRev or "dirty";
            GST_PLUGIN_SYSTEM_PATH_1_0 = lib.makeSearchPath "lib/gstreamer-1.0" gstPlugins;
            LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
          };

          cargoArtifacts = craneLib.buildDepsOnly commonArgs;

          kubecord = craneLib.buildPackage (
            commonArgs
            // {
              inherit cargoArtifacts;
              pname = "kubecord";
              version = "0.1.0";
              # Arguments for the launcher wrapGAppsHook writes. (Set as an
              # attribute they are ignored; the hook's own list is added to.)
              # Go Live sound uses pw-cli, pw-dump and pw-link; the system's
              # own come first, matching its PipeWire.
              preFixup = ''
                gappsWrapperArgs+=(
                  --prefix GST_PLUGIN_SYSTEM_PATH_1_0 : "${lib.makeSearchPath "lib/gstreamer-1.0" gstPlugins}"
                  --suffix PATH : "${lib.makeBinPath [ pkgs.pipewire ]}"
                )
              '';
              meta = {
                description = "A Discord client";
                license = lib.licenses.gpl3Only;
                platforms = lib.platforms.linux;
                mainProgram = "kubecord";
              };
            }
          );
        in
        {
          packages.default = kubecord;

          devShells.default = pkgs.mkShell {
            inputsFrom = [ kubecord ];
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
              # TLS for the captcha view's WebKit (the package's wrapper adds it).
              export GIO_EXTRA_MODULES="${pkgs.glib-networking}/lib/gio/modules:$GIO_EXTRA_MODULES"
            '';
          };

          formatter = pkgs.nixfmt;

          checks = {
            inherit kubecord;
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