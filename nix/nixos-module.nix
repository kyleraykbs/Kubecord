# NixOS module: installs Kubecord system-wide and declares settings for every
# user in /etc/kubecord/settings.json (Home Manager's settings, per user, win
# over these).
{ defaultPackage }:
{ config, lib, pkgs, ... }:
let
  cfg = config.programs.kubecord;
  json = pkgs.formats.json { };
in
{
  imports = [ (import ./options.nix { inherit defaultPackage; }) ];

  config = lib.mkIf cfg.enable {
    environment.systemPackages = lib.optional (cfg.package != null) cfg.package;
    environment.etc."kubecord/settings.json" = lib.mkIf (cfg.settings != { }) {
      source = json.generate "kubecord-settings.json" cfg.settings;
    };
  };
}
