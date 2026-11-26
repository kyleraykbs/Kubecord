# Kubecord on NixOS and Home Manager

Kubecord's flake has a NixOS module and a Home Manager module. Both install
Kubecord and can set any of its settings declaratively, while every setting
you leave out stays yours to change in the app.

- [Adding the flake](#adding-the-flake)
- [NixOS module](#nixos-module)
- [Home Manager module](#home-manager-module)
- [Options](#options)
- [Stylix](#stylix)
- [How declared and app settings combine](#how-declared-and-app-settings-combine)
- [Troubleshooting](#troubleshooting)
- [Settings reference](#settings-reference)

## Adding the flake

Add Kubecord to your system or Home Manager flake's inputs:

```nix
{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    kubecord.url = "github:kyleraykbs/Kubecord";
    # Optional: use your nixpkgs instead of Kubecord's own.
    kubecord.inputs.nixpkgs.follows = "nixpkgs";
  };
}
```

## NixOS module

Import `kubecord.nixosModules.default` and enable it:

```nix
{
  outputs = { nixpkgs, kubecord, ... }: {
    nixosConfigurations.my-machine = nixpkgs.lib.nixosSystem {
      system = "x86_64-linux";
      modules = [
        kubecord.nixosModules.default
        {
          programs.kubecord = {
            enable = true;
            settings = {
              use_24_hour_time = true;
              notify_preview = false;
            };
          };
        }
      ];
    };
  };
}
```

This installs Kubecord for every user and writes the settings to
`/etc/kubecord/settings.json`, where Kubecord reads them for every user.

## Home Manager module

Import `kubecord.homeManagerModules.default`, either in a standalone Home
Manager configuration or in NixOS's `home-manager.users.<name>`:

```nix
{
  home-manager.users.alice = {
    imports = [ kubecord.homeManagerModules.default ];

    programs.kubecord = {
      enable = true;
      settings = {
        follow_system_scheme = false;
        dark_scheme = true;
        discord_font = true;
        transparent_background = true;
        background_opacity = 85;
        logger.viewed = { text = "forever"; media = "year"; };
        sound_volumes.message = 0.5;
      };
    };
  };
}
```

This installs Kubecord for that user and writes the settings to
`~/.config/kubecord/nix-settings.json`.

You can use both modules at once: NixOS for machine-wide defaults, Home
Manager for one person's preferences. Where both set the same value, Home
Manager's wins.

## Options

The same three options exist in both modules.

| Option | Type | Default | Description |
| --- | --- | --- | --- |
| `programs.kubecord.enable` | boolean | `false` | Install Kubecord and apply the settings. |
| `programs.kubecord.package` | package or `null` | this flake's Kubecord | The package to install. `null` installs nothing and only writes the settings, for a Kubecord installed another way. The Flatpak can't see these files from inside its sandbox, so it ignores them. |
| `programs.kubecord.settings` | JSON value (attribute set) | `{ }` | Kubecord settings, by the keys in [the reference below](#settings-reference). Any subset; nested values (like `logger.viewed.media`) can be set one at a time. |
| `programs.kubecord.stylix.enable` | boolean | `true` when Stylix is enabled | Match Kubecord to Stylix (see [Stylix](#stylix)). |

## Stylix

If you use [Stylix](https://github.com/nix-community/stylix), the modules
match Kubecord to it automatically. Nothing to set up: when Stylix is enabled
in the same configuration, they set:

| Setting | Value |
| --- | --- |
| `follow_system_scheme` | `true`, so Kubecord uses the GTK colors Stylix generates. |
| `transparent_background` | `true` when `stylix.opacity.applications` is below `1.0`. |
| `background_opacity` | `stylix.opacity.applications` as a percentage (`0.9` is `90`). |

These are defaults, so anything you put in `programs.kubecord.settings` wins
over them. To turn the matching off entirely, set
`programs.kubecord.stylix.enable = false`.

This works in both modules: the NixOS module follows your system's Stylix,
and the Home Manager module follows the Stylix settings Home Manager sees
(including Stylix's own NixOS-to-Home-Manager integration).

## How declared and app settings combine

Kubecord keeps the settings you change in the app in
`~/.config/kubecord/settings.json`. On start it reads three layers and merges
them key by key, each winning over the one before:

1. what you set in the app (`~/.config/kubecord/settings.json`);
2. the NixOS module (`/etc/kubecord/settings.json`);
3. the Home Manager module (`~/.config/kubecord/nix-settings.json`).

What this means in practice:

- **A declared setting wins on every start.** The settings window marks it
  with a **Nix** badge, and a banner at the top says how many settings your
  configuration sets.
- **You can still change it in the app, for this run.** The badge turns into
  **Temporary**, a notice says the change lasts until restart, and on the next
  start your configuration's value is back. Declared values are never written
  to `settings.json`.
- **Everything you don't declare is the app's.** Change it in the settings
  window and it's saved and kept across restarts, as usual.
- **Removing a setting from your configuration hands it back to the app.** It
  goes back to whatever you last set in the app, or to its default.
- **Nested settings merge too.** Declaring `logger.viewed.media` leaves
  `logger.viewed.text` and the rest of `logger` to the app.

## Troubleshooting

Kubecord ignores a declared setting it can't use, rather than failing to
start, and logs a warning to `~/.cache/kubecord/kubecord.log`:

- `declared setting has the wrong type; ignored`: the value isn't one the
  setting takes, such as a string where a number goes, or a keep time not in
  the list (they're lowercase: `"forever"`, not `"Forever"`).
- `declared setting is not a Kubecord setting; ignored`: a misspelled key, or
  one Kubecord no longer has.

Settings that are dropped this way don't get a Nix badge, which is a quick way
to spot them.

To see what your configuration produced, read the generated file:

```sh
cat /etc/kubecord/settings.json              # NixOS
cat ~/.config/kubecord/nix-settings.json     # Home Manager
```

## Settings reference

Every key below can go in `programs.kubecord.settings`. Defaults are what
Kubecord uses when neither the app nor your configuration sets a value.

### Chat and appearance

| Setting | Default | Meaning |
| --- | --- | --- |
| `use_24_hour_time` | `false` | Show times on a 24-hour clock (`17:58`) instead of `5:58 PM`. |
| `send_typing` | `true` | Tell others when you are typing. |
| `auto_idle` | `false` | Show as Idle after 10 minutes away from the app, until you come back (only from Online). |
| `spellcheck` | `true` | Underline misspelled words in the message box. |
| `unload_distant_messages` | `true` | Unload messages far from where you are reading, to save memory (they reload when you scroll back). |
| `animate_gifs_unfocused` | `false` | Keep GIFs playing while the window is not focused. |
| `follow_system_scheme` | `true` | Use your system's libadwaita colors and light/dark preference. `false` uses Discord's colors (see `dark_scheme`). |
| `dark_scheme` | `true` | Discord's dark (`true`) or light (`false`) theme, when `follow_system_scheme` is `false`. |
| `discord_font` | `false` | Use Discord's font, gg sans (downloaded once from discord.com). |
| `discord_emoji` | `true` | Draw emoji with Discord's artwork instead of your emoji font. |
| `transparent_background` | `false` | Let the desktop show through the window's background. |
| `background_opacity` | `80` | How solid the background is while transparent, in percent (`1` to `100`). |
| `kde_transparency_workaround` | `false` | Tell the compositor it decorates the window (KDE's decoration protocol), so niri draws no backdrop behind a transparent window. Leave off on KDE and sway. |
| `default_account` | `null` | The user ID (as a string) of the account to open on; `null` opens on the first. |
| `captcha_in_app` | `true` | Show Discord's captchas inside Kubecord instead of the browser. |
| `safety_limits` | `true` | Ask before, then stop, actions Discord's spam filter watches when done quickly: new DMs, friend requests, new groups, server joins, and the same message in several channels. Turning it off risks a ban. |

### Notifications

| Setting | Default | Meaning |
| --- | --- | --- |
| `notify_desktop` | `true` | Show desktop notifications. |
| `notify_preview` | `true` | Put the message text in notifications. |
| `notify_all_dms` | `true` | Every DM message notifies (`false`: only mentions in DMs). |
| `notify_mentions_only` | `false` | Only mentions notify, whatever servers and channels say. |
| `notify_everyone` | `true` | `@everyone` and `@here` notify where the server allows them. |
| `notify_roles` | `true` | Mentions of your roles notify where the server allows them. |
| `notify_calls` | `true` | Incoming DM and group calls notify. |
| `open_channel_quiet` | `"while_focused"` | When the channel you have open stays quiet: `"while_focused"` (as Discord does), `"always"` or `"never"`. |
| `notify_mark_window` | `true` | Show the number of new notifications in the window title until you look at it. |

### Sounds

| Setting | Default | Meaning |
| --- | --- | --- |
| `sounds_enabled` | `true` | Play interface sounds at all. |
| `sound_volume` | `0.6` | Their overall volume, `0.0` to `1.0`. |
| `sounds_off` | `[ ]` | Sounds switched off, by key (`"message"`, `"user_join"`, …; the keys are listed on the Sounds page's sounds). |
| `sound_volumes` | `{ }` | Each sound's own volume, `0.0` to `1.0`, by key, on top of `sound_volume`; unlisted sounds play at `1.0`. |

### Voice

| Setting | Default | Meaning |
| --- | --- | --- |
| `input_device` | `""` | Microphone, by its PipeWire node name; `""` for the system default. |
| `output_device` | `""` | Speakers or headphones, by node name; `""` for the default. |
| `input_volume` | `1.0` | Microphone volume, `0.0` to `2.0`. |
| `output_volume` | `1.0` | Speaker volume, `0.0` to `2.0`. |
| `noise_suppression` | `0` | `0` off, `1` standard (WebRTC), `2` strong (RNNoise, where installed). |
| `echo_cancellation` | `false` | Remove your speakers' sound from your microphone. |
| `auto_gain_control` | `false` | Even out your microphone's loudness. |
| `auto_input_sensitivity` | `true` | Let Kubecord decide what is speech; `false` uses `input_threshold_db`. |
| `input_threshold_db` | `-50.0` | Microphone sound quieter than this (dBFS, `-100` to `0`) is not sent. |
| `volume_boost_cap` | `3` | The most one person's volume can be turned up, as a multiple (`1` to `5`). |
| `playback_buffer_ms` | `60` | Other people's sound collected before it starts playing, in ms: `20`, `40`, `60`, `80`, `100`, `150` or `200`. |
| `late_sound_wait_ms` | `40` | How long playback waits for late sound instead of skipping it, in ms: `0` (don't wait), `40`, `100` or `200`. |
| `playback_settle_ms` | `2000` | How long sound must arrive steadily before delay added by waiting is shed again, in ms: `1000`, `2000`, `5000`, `10000` or `0` (never). |
| `playback_buffer_max_ms` | `300` | The most sound that may wait before playback skips ahead to catch up, in ms: `200`, `300`, `500` or `1000`. |

### Camera and screen share

| Setting | Default | Meaning |
| --- | --- | --- |
| `camera` | `""` | The camera to use, by name; `""` to ask each time. |
| `camera_format` | `0` | Camera capture format: `0` automatic, `1` MJPEG, `2` raw. |
| `always_preview` | `false` | Start the camera preview as soon as the camera popup opens. |
| `self_preview` | `true` | Show your own camera and stream in the call. |
| `share_source` | `0` | What the screen-share picker offers: `0` screens and windows, `1` screens, `2` windows. |
| `share_audio` | `1` | A stream's sound: `0` none, `1` everything playing (except Kubecord), `2` one program, `3` a node for a patchbay. |
| `share_audio_app` | `null` | The program shared when `share_audio` is `2` (as Kubecord records it; easiest to pick in the app). |
| `share_audio_app_name` | `null` | That program's display name. |
| `stream_audio_gain` | `1.0` | Boost on your stream's sound for viewers, as a multiplier (`1.0` unchanged). |
| `width` | `1280` | Video width in pixels; with `height`, one of 1920×1080, 1280×720, 960×540 or 640×360 (anything else falls back to 1280×720). |
| `height` | `720` | Video height in pixels (see `width`). |
| `fps` | `30` | Framerate: `60`, `30`, `24` or `15`. |
| `fps_auto` | `false` | Let the framerate follow the network, between `fps_auto_min` and `fps_auto_max`. |
| `fps_auto_min` | `15` | Lowest automatic framerate: `5`, `10`, `15`, `20`, `24` or `30`. |
| `fps_auto_max` | `30` | Highest automatic framerate: `60`, `30`, `24` or `15`. |
| `fps_auto_target` | `null` | Framerate while the connection is clean; `null` for the highest. |
| `bitrate_kbps` | `null` | Fixed bitrate in kbit/s; `null` lets it follow the network. |
| `auto_bitrate_min` | `null` | Lowest automatic bitrate in kbit/s (`300`, `500`, `800`, `1000`, `1500`, `2000`, `3000` or `4000`); `null` for three tenths of the top. |
| `auto_bitrate_max` | `null` | Highest automatic bitrate in kbit/s; `null` to work it out from resolution and framerate. |
| `auto_bitrate_target` | `null` | Bitrate while the connection is clean; `null` for the highest. |
| `adapt_priority` | `0` | With both automatic, what is kept when the connection struggles: `0` balanced, `1` bitrate, `2` framerate. |
| `go_live_delay_ms` | `0` | Fixed stream buffering delay in milliseconds. |
| `cpu_encoder` | `true` | Use the software x264 encoder instead of trying the GPU (VA-API) first. |
| `stream_keyframe_secs` | `0` | Seconds between scheduled keyframes; `0` sends them only when needed. |
| `full_frame_gap_secs` | `0` | Send a full frame when none has gone out for this many seconds; `0` only when needed. |
| `loss_keyframe_ms` | `1000` | Least time between keyframes sent because packets were lost, in ms. |
| `drop_frames_after_ms` | `250` | Drop a frame that waited this long beyond the delay; `0` never drops. |
| `leaky_queue` | `true` | Drop old frames in the capture queue instead of building up latency. |
| `retransmit_ms` | `2000` | How long sent packets are kept to repair a dropout, in ms; `0` disables it. |
| `send_twice` | `false` | Send every video packet a second time, 20 ms later. |

### Message logger (`logger`)

Each kind of message has a keep time for its text and its media: one of `"off"`, `"day"`, `"week"`, `"month"`, `"half_year"`, `"year"`, `"forever"`.

| Setting | Default | Meaning |
| --- | --- | --- |
| `logger.viewed.text` | `"forever"` | Messages in the channel you have open: how long to keep the text. |
| `logger.viewed.media` | `"half_year"` | Messages in the channel you have open: how long to keep the media. |
| `logger.notifying.text` | `"year"` | Messages that would notify you: how long to keep the text. |
| `logger.notifying.media` | `"week"` | Messages that would notify you: how long to keep the media. |
| `logger.other.text` | `"week"` | Everything else: how long to keep the text. |
| `logger.other.media` | `"off"` | Everything else: how long to keep the media. |
| `logger.per_message_mb` | `20` | Most media kept from one message, in MB; larger attachments are left out. `0` is no limit. |
| `logger.week_cap_mb` | `1024` | Most the log may hold of what arrived in the last week, in MB. `0` is no limit. |
| `logger.month_cap_mb` | `2048` | Most the log may hold of what arrived in the last month, in MB. `0` is no limit. |
| `logger.year_cap_mb` | `20480` | Most the log may hold of what arrived in the last year, in MB. `0` is no limit. |
| `logger.collect.viewed` | `false` | Whether the size limits may remove `viewed` messages. |
| `logger.collect.notifying` | `false` | Whether the size limits may remove `notifying` messages. |
| `logger.collect.other` | `true` | Whether the size limits may remove `other` messages. |

### Logging and window

| Setting | Default | Meaning |
| --- | --- | --- |
| `log_to_file` | `true` | Write a log file in `~/.cache/kubecord/`. |
| `log_detailed` | `false` | Also log every media and voice event (far larger logs). |
| `log_diagnostics` | `false` | Log performance diagnostics (main-thread stalls). |
| `sidebar_width` | `340` | Width of the channel list in pixels. |
| `call_height` | `320` | Height of a call above a DM's chat, in pixels. |
