# Home Manager module: installs Kubecord for the user and declares settings in
# ~/.config/kubecord/nix-settings.json (these win over the NixOS module's).
{ defaultPackage }:
{ config, lib, pkgs, ... }:
let
  cfg = config.programs.kubecord;
  json = pkgs.formats.json { };
in
{
  imports = [ (import ./options.nix { inherit defaultPackage; }) ];

  config = lib.mkIf cfg.enable {
    home.packages = lib.optional (cfg.package != null) cfg.package;
    xdg.configFile."kubecord/nix-settings.json" = lib.mkIf (cfg.settings != { }) {
      source = json.generate "kubecord-nix-settings.json" cfg.settings;
    };
  };
}
