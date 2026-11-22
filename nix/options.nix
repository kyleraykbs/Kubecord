# Options shared by the NixOS and Home Manager modules. `defaultPackage`
# maps a system (x86_64-linux, ...) to this flake's Kubecord.
{ defaultPackage }:
{ lib, pkgs, ... }:
let
  json = pkgs.formats.json { };
in
{
  options.programs.kubecord = {
    enable = lib.mkEnableOption "Kubecord, a native Discord client";

    package = lib.mkOption {
      type = lib.types.nullOr lib.types.package;
      default = defaultPackage pkgs.stdenv.hostPlatform.system;
      defaultText = lib.literalExpression "kubecord.packages.\${pkgs.stdenv.hostPlatform.system}.default";
      description = ''
        The Kubecord package to install. Set to `null` to only declare
        settings, for a Kubecord installed some other way (the Flatpak reads
        the same files only from inside its sandbox, so it does not apply).
      '';
    };

    settings = lib.mkOption {
      type = json.type;
      default = { };
      example = lib.literalExpression ''
        {
          use_24_hour_time = true;
          follow_system_scheme = false;
          dark_scheme = true;
          discord_font = true;
          transparent_background = true;
          background_opacity = 85;
          notify_preview = false;
          logger = {
            viewed = { text = "forever"; media = "year"; };
            week_cap_mb = 2048;
          };
          sound_volumes.message = 0.5;
        }
      '';
      description = ''
        Kubecord settings, by the keys of its `settings.json`; see the
        settings reference (docs/nix.md in the Kubecord repository). Any
        subset can be given, nested objects key by key.

        Declared settings are merged over the ones set in the app: they win,
        and changing one in the app lasts only until Kubecord restarts (the
        settings window marks them "Nix", and "Temporary" once changed). Every
        setting not declared here stays the app's to change and is saved as
        usual.
      '';
    };
  };
}
