<p align="center">
  <img src="data/icons/hicolor/256x256/apps/dev.kyle.kubecord.png" width="128" alt="Kubecord logo">
</p>

# Kubecord

A Discord client for Linux, built from scratch in Rust!

The Linux community has been shackled by the official Discord client for far too long! I thought someone else would do it, but I guess if you want something done right (or done at all) you gotta do it yourself. Welcome to your liberation from Electron! Please open issues if you have suggestions for features or find any bugs. I've tested the client extensively, but I'm sure I missed *something*.

- No Electron, no webview, no patched official client: native GTK4/libadwaita.
- Voice, video and screen share (with audio) through Discord's own voice protocol, end-to-end encrypted.
- Follows your GTK theme (Stylix compatible), or uses Discord's own look.

## Quick start

```sh
nix run github:kyleraykbs/Kubecord
```

Or from a checkout: `nix run`. Log in with your email and password (2FA and
SMS supported) or paste a token. See [Build and run](#build-and-run) for
development.

## Warning
Third-party clients are *kinda* (see below for more information) against Discord's TOS
and *can* get the account banned. The risk is quite low though, as Discord mainly tries
to detect self-botting and mass friending for scam prevention reasons.

This client does its best to adhere to the proper usage of the API. See below for more details.
<details>
<summary>Click to expand</summary>

The specific lines in the Discord TOS that this *may* violate and a description of how:

**"using any unauthorized software designed to modify the services"**
```
While there technically are modifications to the original intent, they are minor.
Many other projects like Vesktop and Vencord allow for plugin modifications that
do much more. Those clients and modifications are generally considered low risk.

This project is slightly more risky as we are recreating the API interaction
in full rather than just tacking onto the existing app.

Our modifications are basic and attempt to be respectful. To get an idea, here are some:
- Hide whether you're typing
- See stream details
- Search GIF favorites
- Keep a local log of deleted and edited messages (only what Discord already
  sends the client; nothing is fetched to log it)
```

**"scraping our services without our written consent, including by using any robot, spider, crawler, scraper, or other automatic device, process, or software"**
```
Our client doesn't automate any scraping or messaging. It simply lets the *user*
interact with Discord just like they normally would.
```

**"copying, dismantling, or reverse engineering any of our services or using our intellectual property without permission"**
```
You aren't reverse engineering anything while using this client, although we
did use *other people's* clients as a reference, and reverse engineering was
required in the making of them. We never reverse engineered the Discord app itself.

Clients like Vesktop and Vencord did the same.
```

**"trying to gain access to, intentionally overburdening, or attacking our systems"**
```
Our client doesn't try to bypass any rate limits or flood Discord's servers. If a
limit isn't enforced on the client's end yet, though, it *could* look like that on
Discord's end, say if you spam somewhere with a rate limit I haven't implemented.
```

In summary, we aren't doing anything malicious, and that's where the main ban risk is. Discord tends to ban for MASS calls or really suspicious API behavior. Our client tries to be respectful to the API, but I can't promise no call will ever get flagged. From my own use, however, it seems very unlikely. Be respectful and a ban is unlikely. Friending people is the most likely place to get a ban.
</details>


## Features
### 1 - Voice and video
- Send/receive audio
    - See who is talking
    - Per-user volume, boosted up to 5x
    - Discord's own join, leave, mute and deafen sounds
- Screen share & webcam
    - With audio!!
      - Whole desktop
      - Specific window
      - Dummy node for any patchbay
    - The screen-share preview shows the *encoded* video stream: what you see is what others see (barring connection stutters)
    - Debug details for streams you share *and* streams you watch
    - MANY fine-tunable settings (you're in full control)
- DM and group DM calls
    - Shows when the other party is being rung
- Noise suppression, echo cancellation, automatic gain and input sensitivity

### 2 - Chat
- Send, edit, reply, react, pin
- Media
  - Images, videos and GIFs, with a zooming viewer
  - Right-click any media to copy its link, open it in the browser or save it
  - Embeds and attachments (with download)
  - Previews of files before you send them
- GIFs
    - All types: MP4, GIF, etc.
    - Favorites
    - Search (including partial favorites search, matched by link)
- Emoji
    - Server & standard emoji, with a picker
    - Drawn with Discord's own artwork (optional), even while typing
    - `:shortcode:` emoji as you type
- Stickers (all except Discord's stock stickers)
- Spellcheck, with suggestions on right-click
- Typing indicators
    - Option to hide your own typing
- Apps and bots
    - Slash commands, with autocomplete and file options
    - Buttons, select menus and forms (pop-up dialogs)
    - App commands on users and messages (right-click)
- Message logger (local only, configurable)
    - Keeps deleted messages, shown in red, and edit history
    - Separate keep times for text and media, by kind (viewed channel, would notify, everything else)
    - Size limits per message, week, month and year, with the largest media dropped first
- Share mode: pick a range of messages (redact or add single ones with right-click) and export it as a picture or as text, to the clipboard or a file
- All available admin tools, including channel and server settings

### 3 - Profiles
- Edit your own profile
- View other people's profiles
- Status, notes & bio
- Mutual friends and servers
- Basic activity support

### 4 - The whole app
- Not built in Electron! (Biggest feature of them all)
- Built in GTK4/libadwaita
- Multiple accounts, with a default account to open on
- Notifications
- Login
  - Email & password
  - Token
  - 2FA
  - SMS verification
  - Captcha support
- Theming
  - Follows all GTK colors (Stylix compatible), or Discord's own dark and light themes
  - Discord's font (optional)
  - Transparent background with adjustable opacity
- Desktop entry and icon
- Extensive settings

## Unsupported
- Discord's web activities, like its golf game, chess, wordle, etc.
- Animated profile overlays
- Lottie (Discord's stock) stickers

## Settings worth knowing about
- **Theming → Transparent Background**: on niri, the compositor's border color
  can show through the whole window. Turn on **KDE Transparency Workaround**
  next to it, or add `draw-border-with-background false` to a niri window rule
  for `app-id="^dev\.kyle\.kubecord$"`. Leave the workaround off on KDE and sway,
  which would draw a second title bar.
- **Message Logger**: on by default (viewed channels' text kept forever,
  everything else for less; see its tab). Set a kind to *Don't Log* to stop
  logging it. Logged messages stay on your machine only.
- **Account menu** (your name at the bottom left): the star next to an account
  makes it the one Kubecord opens on.

## Where things are kept
| What | Where |
| --- | --- |
| Accounts (tokens, mode 0600) | `$XDG_CONFIG_HOME/kubecord/accounts.json` |
| Settings | `$XDG_CONFIG_HOME/kubecord/settings.json` |
| Message log | `$XDG_DATA_HOME/kubecord/logger/` |
| Image, emoji and font cache, log file | `$XDG_CACHE_HOME/kubecord/` |

## Technical details
The UI is GTK4/libadwaita and inherits the system Adwaita colours, so it follows
your GTK theme. Media is GStreamer. Voice uses Discord's UDP voice protocol directly:
SRTP with `aead_xchacha20_poly1305_rtpsize` (or AES-256-GCM when offered) and
DAVE end-to-end encryption through the `davey` crate. There is no Electron, no
webview and no patched official client.

## Build and run
NixOS/Nix
```sh
nix run
```

Development
```sh
nix develop
cargo run -p kubecord
```

The flake provides GTK4, libadwaita, GStreamer with the plugins the client uses,
libopus and the Rust toolchain (1.92+). The packaged build wraps
`GST_PLUGIN_SYSTEM_PATH_1_0` and the GTK schemas, and installs the desktop entry
and icons. New files need `git add` before `nix build`, since the flake only sees
tracked files. A Flatpak is planned.

## Logging in

Add an account from the login page with either an **email and password** (with a
two-factor code when the account requires one) or a **user token** directly.
Discord may ask for a captcha or an email confirmation for a login from a
third-party client. Only the resulting token is kept; the password and any codes
are never stored or logged. Multiple accounts are supported; each account owns
its gateway connection, REST client and at most one voice connection.

## Development tools

`kc-probe` is the protocol probe used during development:

```sh
KUBECORD_TOKEN=<token> cargo run -p kc-probe -- user
KUBECORD_TOKEN=<token> cargo run -p kc-probe -- guilds
KUBECORD_TOKEN=<token> cargo run -p kc-probe -- channels <guild-id>
KUBECORD_TOKEN=<token> cargo run -p kc-probe -- messages <channel-id>
KUBECORD_TOKEN=<token> cargo run -p kc-probe -- watch
KUBECORD_TOKEN=<token> cargo run -p kc-probe -- voice <guild-id> <channel-id>
KUBECORD_TOKEN=<token> cargo run -p kc-probe -- voice <guild-id> <channel-id> --stream --screen
```

`kc-probe voice` joins a channel, speaks/receives, and with `--stream --screen`
starts a Go Live screen share through the XDG desktop portal. Add `--no-audio`
to test the voice handshake without opening the microphone or speaker.

Go Live includes desktop audio. It is captured from the default sink monitor
(`pulsesrc device=@DEFAULT_MONITOR@`); set `KUBECORD_MONITOR_DEVICE` to a
specific monitor source if you route application audio through a virtual sink.
Capturing the default monitor also captures the Discord client's own output, so
use a virtual sink for a clean stream.

`KUBECORD_DEMO=<mode>` runs the interface against a made-up offline account
(no Discord connection), for trying out and screenshotting the UI; the modes
are listed at the top of `crates/kc-ui/src/demo.rs`.

## Layout

```
crates/kc-core      snowflakes, accounts, tokens, super-properties
crates/kc-http      REST v9, rate limits, uploads, app commands
crates/kc-gateway   user gateway, heartbeat, identify/resume
crates/kc-state     Discord models and the event reducer
crates/kc-voice     voice gateway, UDP/SRTP/RTP, DAVE, streams
crates/kc-media     GStreamer capture, Opus, H.264, rendering
crates/kc-ui        GTK4/libadwaita application
crates/kc-probe     protocol probe CLI
crates/kubecord     application entry point
data/               desktop entry and icons
```

## License
GPL-3.0-only, see [LICENSE](LICENSE). Copyright (c) 2026 kyleraykbs.

Kubecord is free software: you can redistribute it and/or modify it under the
terms of the GNU General Public License as published by the Free Software
Foundation, version 3. It is distributed in the hope that it will be useful,
but WITHOUT ANY WARRANTY; without even the implied warranty of MERCHANTABILITY
or FITNESS FOR A PARTICULAR PURPOSE. See the GNU General Public License for
more details.
