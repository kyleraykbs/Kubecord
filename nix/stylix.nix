# Matches Kubecord to Stylix when Stylix is enabled: the system colors (which
# Stylix themes through GTK) and Stylix's application opacity as Kubecord's
# transparent background. Set as defaults, so programs.kubecord.settings wins.
{ config, lib, options, ... }:
let
  cfg = config.programs.kubecord;
  stylixEnabled = (options ? stylix) && (config.stylix.enable or false);
  opacity = config.stylix.opacity.applications or 1.0;
  percent = lib.max 1 (lib.min 100 (builtins.floor (opacity * 100 + 0.5)));
in
{
  options.programs.kubecord.stylix.enable = lib.mkOption {
    type = lib.types.bool;
    default = stylixEnabled;
    defaultText = lib.literalMD "`true` when Stylix is enabled";
    description = ''
      Match Kubecord to Stylix: follow the system color scheme (Stylix's
      colors) and use `stylix.opacity.applications` for Kubecord's
      transparent background. These are defaults: anything set in
      `programs.kubecord.settings` wins.
    '';
  };

  config = lib.mkIf (cfg.enable && cfg.stylix.enable) {
    programs.kubecord.settings = lib.mapAttrs (_: lib.mkDefault) (
      {
        follow_system_scheme = true;
        transparent_background = opacity < 1.0;
      }
      // lib.optionalAttrs (opacity < 1.0) { background_opacity = percent; }
    );
  };
}
