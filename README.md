<div align="center">

<img src="data/icons/hicolor/256x256/apps/io.github.kyleraykbs.Kubecord.png" width="112" alt="Kubecord logo">

# Kubecord

**A Discord client for Linux, built from scratch in Rust.**

Native GTK4/libadwaita. No Electron, no webview, no patched official client, FULL screen-share support.

[![License: GPL v3](https://img.shields.io/badge/license-GPL--3.0--only-blue.svg)](LICENSE)
![Rust](https://img.shields.io/badge/rust-1.92%2B-orange.svg)
![GTK4](https://img.shields.io/badge/GTK-4%20%2B%20libadwaita-4a86cf.svg)
![Nix](https://img.shields.io/badge/built%20with-Nix-5277c3.svg)

<img width="820" alt="image" src="https://github.com/user-attachments/assets/35548897-e369-4387-ac0c-30f3ad3ef592" alt="Kubecord: a server's channels, a chat and the member list"/>

</div>

---

The Linux community has been shackled by the official Discord client for far too long! I thought someone else would do it, but I guess if you want something done right (or done at all) you gotta do it yourself. Welcome to your liberation from Electron!

Please open issues if you have suggestions for features or find any bugs. I've tested the client extensively, but I'm sure I missed *something*.

## Contents

- [Installation](#installation)
- [Is it safe to use?](#is-it-safe-to-use)
- [Features](#features)
- [Not supported yet](#not-supported-yet)
- [Building](#building)
- [Files](#files)
- [Development](#development)
- [License](#license)

## Installation

Pick the one for your system and open it for the steps.

| Package | For |
| --- | --- |
| **Flatpak** | Any distribution |
| **AUR** (`kubecord-git`) | Arch Linux and its derivatives |
| **.deb** | Debian 13, Ubuntu 25.04 and newer |
| **AppImage** | Ubuntu 26.04, Fedora 44, Arch and newer |
| **Nix** | NixOS, or Nix on any distribution |
| **From source** | Anything else |

<details>
<summary><b>Flatpak</b>: any distribution</summary>

<br>

Install it from Kubecord's own Flatpak repository, which also adds the
repository so `flatpak update` keeps it current:

```sh
flatpak install --user https://kyleraykbs.github.io/Kubecord/io.github.kyleraykbs.Kubecord.flatpakref
```

Or add the repository first, then install:

```sh
flatpak remote-add --user --if-not-exists flathub https://dl.flathub.org/repo/flathub.flatpakrepo
flatpak remote-add --user --if-not-exists kubecord https://kyleraykbs.github.io/Kubecord/kubecord.flatpakrepo
flatpak install --user kubecord io.github.kyleraykbs.Kubecord
```

Updates are signed with the repository's key. Each
[release](https://github.com/kyleraykbs/Kubecord/releases) also has a
`Kubecord.flatpak` file you can install directly with
`flatpak install --user Kubecord.flatpak` (without automatic updates).

</details>

<details>
<summary><b>AUR</b>: Arch Linux, Manjaro, EndeavourOS, …</summary>

<br>

> [!NOTE]
> AUR registration is closed at the moment, so `kubecord-git` isn't on the
> AUR yet. Until it is, build the same package from this repository, or use
> [Nix](#installation) (it works on Arch too).

Build and install it with the PKGBUILD in this repository:

```sh
git clone https://github.com/kyleraykbs/Kubecord.git
cd Kubecord/packaging/aur/kubecord-git
makepkg -si
```

Once it's published, it will install with your AUR helper:

```sh
paru -S kubecord-git    # or: yay -S kubecord-git
```

</details>

<details>
<summary><b>.deb</b>: Debian 13, Ubuntu 25.04 and newer</summary>

<br>

Download `kubecord_*_amd64.deb` from the latest
[release](https://github.com/kyleraykbs/Kubecord/releases) and install it,
which also pulls in the GStreamer plugins it needs:

```sh
sudo apt install ./kubecord_*_amd64.deb
```

On niri and other compositors like it, Debian 13's own GTK crashes at start;
use the Flatpak there.

</details>

<details>
<summary><b>AppImage</b>: Ubuntu 26.04, Fedora 44, Arch and newer</summary>

<br>

Download `Kubecord-x86_64.AppImage` from the latest
[release](https://github.com/kyleraykbs/Kubecord/releases), then:

```sh
chmod +x Kubecord-x86_64.AppImage
./Kubecord-x86_64.AppImage
```

The AppImage has no captcha view: if Discord asks for a captcha when you log
in with email and password, log in with a token instead.

</details>

<details>
<summary><b>Nix</b>: NixOS, or Nix on any distribution</summary>

<br>

Run it without installing:

```sh
nix run github:kyleraykbs/Kubecord
```

Install it into your profile:

```sh
nix profile install github:kyleraykbs/Kubecord
```

Or use the NixOS or Home Manager module, which can also set Kubecord's
settings declaratively (any you leave out stay changeable in the app):

```nix
# In your flake's inputs:
kubecord.url = "github:kyleraykbs/Kubecord";

# NixOS: in your modules
kubecord.nixosModules.default
{ programs.kubecord.enable = true; }

# Home Manager: in your imports
kubecord.homeManagerModules.default
{ programs.kubecord = { enable = true; settings.use_24_hour_time = true; }; }
```

See [docs/nix.md](docs/nix.md) for the options, how declared and in-app
settings combine, and every setting.

</details>

<details>
<summary><b>From source</b>: anything else</summary>

<br>

You need Rust 1.92 or newer, clang, and the development packages for GTK 4.14
or newer, libadwaita 1.7 or newer, WebKitGTK 6.0, PipeWire, Opus and GStreamer
(base, good, bad, ugly and libav, plus the gtk4 and pipewire plugins).

```sh
git clone https://github.com/kyleraykbs/Kubecord.git
cd Kubecord
cargo build --release -p kubecord
./target/release/kubecord
```

</details>

Then log in with your **email and password** (2FA, SMS codes and captchas are
handled) or paste a **user token**. Only the resulting token is kept: your
password and codes are never stored or logged.

## Is it safe to use?

> [!WARNING]
> Third-party clients are *kinda* against Discord's Terms of Service and *can*
> get an account banned. The risk is quite low, as Discord mainly looks for
> self-botting and mass friending (for scam prevention). Kubecord does its best
> to use the API the way the official client does.

<details>
<summary><b>The Terms of Service lines this <i>may</i> touch, and why the risk is low</b></summary>

<br>

**"using any unauthorized software designed to modify the services"**

While there technically are modifications to the original intent, they are
minor. Many other projects like Vesktop and Vencord allow for plugin
modifications that do much more, and those are generally considered low risk.
This project is slightly more risky, as it recreates the API interaction in full
rather than tacking onto the existing app. Its modifications are basic and try
to be respectful, for example:

- Hide whether you're typing
- See stream details
- Search GIF favorites
- Keep a local log of deleted and edited messages (only what Discord already
  sends the client; nothing is fetched to log it)

**"scraping our services without our written consent, including by using any robot, spider, crawler, scraper, or other automatic device, process, or software"**

Kubecord doesn't automate any scraping or messaging. It simply lets *you*
interact with Discord just like you normally would.

**"copying, dismantling, or reverse engineering any of our services or using our intellectual property without permission"**

You aren't reverse engineering anything by using this client. We did use
*other people's* clients as a reference, and reverse engineering was required to
make them, but we never reverse engineered the Discord app itself. Clients like
Vesktop and Vencord did the same.

**"trying to gain access to, intentionally overburdening, or attacking our systems"**

Kubecord doesn't try to bypass any rate limits or flood Discord's servers. If a
limit isn't enforced on the client's side yet, though, it *could* look like that
to Discord, say if you spam somewhere with a rate limit I haven't implemented.

**In summary:** nothing here is malicious, and that's where the main ban risk
is. Discord tends to ban for MASS calls or really suspicious API behavior.
Kubecord tries to be respectful to the API, but I can't promise no call will
ever get flagged. From my own use, it seems very unlikely. Be respectful and a
ban is unlikely; friending people is the most likely place to get one.

</details>

<details>
<summary><b>What Kubecord does to avoid getting flagged</b></summary>

<br>

**It looks like Discord's website in Chrome.** Kubecord was checked against
captures of the real website, request by request:

- **The connection itself**: TLS through BoringSSL (Chrome's own library) with
  Chrome's handshake, and Chrome's HTTP/2 settings. A typical Rust or Python
  client is easy to tell apart from a browser before it sends a single request.
- **The identity**: the current Chrome version (checked against Google at
  startup) and Discord's current build number (read from discord.com), with
  the same headers, cookies and session details the website sends.
- **The gateway**: the website's login message and features, its heartbeats
  (including whether you're at the app), its compression, and the same
  messages at the same moments (opening a server, a DM, a call).
- **Each action**: sending, uploading, reacting, editing, slash commands,
  friend requests and joining servers send the same fields the website does,
  including where in the app they came from.
- **How much it fetches**: messages are loaded in the website's page sizes,
  profiles are kept for a few minutes instead of being fetched on every click,
  and member searches wait for you to stop typing.

**Safety limits.** The quickest way to get banned is doing what spammers do:
lots of new DMs, friend requests or server joins in a short time, the same
message in many channels, or going through many profiles. Kubecord asks
before you do these quickly, and stops past a limit until enough time has
passed. They can be turned off in Settings → Advanced, behind a big warning.

**Things it can't hide:**

- Kubecord doesn't send the website's analytics, shop, or quest requests.
  Plenty of people block analytics anyway, and faking them would be riskier.
- Several accounts signed in to one Kubecord share an IP address and an
  install, so Discord can tell they're on the same machine (just like
  several accounts in one browser). If one gets banned, the others may be
  looked at too.

</details>

## Features

### Voice and video
- **Voice calls**: send and receive audio, and see who's talking.
- **Per-user volume**: turn anyone up or down, boosted up to 5×.
- **Call sounds**: Discord's own join, leave, mute and deafen sounds with individual volume adjustment.

  <img src="https://github.com/user-attachments/assets/e46c6fed-7ef3-4d15-b2d2-158508b4c13e" width="653" alt="A voice call">

- **Screen share and webcam, with audio**: share the whole desktop, one window,
  or a dummy node for any patchbay.

  <img src="https://github.com/user-attachments/assets/9b550203-9f33-472f-8f1e-3b2de301846b" width="360" alt="Screen-share audio options">

- **Honest preview**: the screen-share preview shows the *encoded* stream, so
  what you see is what others see (barring connection stutters).
- **Stream details**: debug info for streams you share *and* streams you watch.
- **Fine-tuning**: MANY stream settings, so you're in full control.
- **DM and group calls**: including showing when the other side is being rung.
- **Voice processing**: noise suppression, echo cancellation, automatic gain and
  input sensitivity.

  <img src="https://github.com/user-attachments/assets/74427ba5-3679-47b1-b78c-f0f7fddfb376" width="420" alt="Voice settings">

### Chat
- **Messages**: send, edit, reply, react and pin.
- **Long messages**: a counter shows the length limit, and anything longer is
  sent as several messages automatically.
- **Media**: images, videos and GIFs with a zooming viewer; right-click any of
  them to copy the link, open it in a browser or save it.
- **Attachments**: embeds and files (with download), and previews of files
  before you send them.
- **GIFs**: every kind (MP4, GIF, …), with favorites and search (including
  partial favorites search, matched by link).
- **Emoji**: server and standard emoji with a picker, `:shortcode:` emoji as
  you type, and optionally Discord's own emoji artwork, even in the message box.
- **Stickers**: all except Discord's stock ones.
- **Spellcheck**: with suggestions on right-click.
- **Typing indicators**: with an option to hide your own.
- **Apps and bots**: slash commands with autocomplete and file options;
  buttons, select menus and forms; app commands on users and messages.
- **Message logger**: keeps deleted messages (shown in red) and edit history,
  locally only, with separate keep times for text and media, and size limits
  that drop the largest media first.
- **Share mode**: pick a range of messages, redact or add single ones, and
  export it as a picture or text, to the clipboard or a file.
- **Admin tools**: everything available, including channel and server settings.

### Profiles
- **Your profile**: edit it, including status, notes and bio.
- **Other people's profiles**: view anyone's, with their status, bio and notes.
- **Mutuals**: friends and servers you share.
- **Activities**: basic activity support.

### The app
- **No Electron**: native GTK4 and libadwaita, the biggest feature of them all!
- **Multiple accounts**: with a default account to open on.
- **Notifications**: desktop notifications, with per-server and per-channel
  settings.
- **Settings**: extensive, and settable from Nix too (see [docs/nix.md](docs/nix.md)).
- **Theming**: follows your GTK colors (Stylix compatible), or uses Discord's
  own dark and light themes, with Discord's font optional.
- **Transparency**: a see-through background with adjustable opacity.

  <img width="600" alt="Kubecord with a transparent background" src="https://github.com/user-attachments/assets/ab7c98be-4eb6-4d38-b36c-92a870cdda3d" />

## Not supported yet

- Starting threads, and the list of archived threads (existing ones show in
  the sidebar)
- Forum and media channels: their post list, and making posts
- Polls: showing them, voting and creating them
- An inbox of recent mentions
- Forwarding messages (forwards you receive do show)
- Sending stickers (stickers you receive do show)
- Push-to-talk and voice keybinds
- Voice messages: recording them, and their waveform
- Server events
- Stage channels: asking to speak, and moving between speakers and audience
- Soundboard
- Message requests from people who aren't your friends
- Sending super reactions
- Uploading server emoji and stickers
- AutoMod and onboarding settings
- Discord's web activities (its golf game, chess, wordle, …)
- Animated profile overlays
- Lottie stickers (Discord's stock stickers)

## Building

With Nix (recommended):

```sh
nix run              # build and run
nix build            # build; the result also installs the desktop entry and icons
nix develop          # a shell with everything for cargo
cargo run -p kubecord
```

The flake provides GTK4, libadwaita, GStreamer with the plugins Kubecord uses,
libopus and the Rust toolchain (1.92+). The packaged build wraps the GStreamer
plugin path and GTK schemas. New files need `git add` before `nix build`, as the
flake only sees tracked files.

Packaging lives in `flatpak/` (the Flatpak manifest; run
`python3 flatpak/cargo-sources.py` after changing `Cargo.lock`) and
`packaging/` (the AUR PKGBUILD, the .deb and AppImage build scripts, and the
Flatpak repository's files). Pushing a `v*` tag runs
`.github/workflows/release.yml`, which builds the .deb, AppImage and Flatpak,
publishes the Flatpak repository on GitHub Pages and makes a release.

## Files

| What | Where |
| --- | --- |
| Accounts (tokens, mode 0600) | `$XDG_CONFIG_HOME/kubecord/accounts.json` |
| Settings | `$XDG_CONFIG_HOME/kubecord/settings.json` |
| Message log | `$XDG_DATA_HOME/kubecord/logger/` |
| Image, emoji and font cache; log file | `$XDG_CACHE_HOME/kubecord/` |

Tokens are stored in plain text on purpose (a local, single-user client; a
keyring would add a daemon requirement), readable only by you.

## Development

### How it works

The UI is GTK4/libadwaita and inherits the system Adwaita colors, so it follows
your GTK theme. Media is GStreamer. Voice uses Discord's UDP voice protocol
directly: SRTP with `aead_xchacha20_poly1305_rtpsize` (or AES-256-GCM when
offered) and DAVE end-to-end encryption through the `davey` crate. Each account
has its own gateway connection, REST client and at most one voice connection.

### Layout

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

### Demo mode

`KUBECORD_DEMO=<mode>` runs the interface against a made-up, offline account (no
Discord connection), for trying things out and taking screenshots. The modes are
listed at the top of `crates/kc-ui/src/demo.rs`.

```sh
KUBECORD_DEMO=default cargo run -p kubecord
```

### Protocol probe

`kc-probe` talks to Discord directly, without the interface:

```sh
KUBECORD_TOKEN=<token> cargo run -p kc-probe -- user
KUBECORD_TOKEN=<token> cargo run -p kc-probe -- guilds
KUBECORD_TOKEN=<token> cargo run -p kc-probe -- channels <guild-id>
KUBECORD_TOKEN=<token> cargo run -p kc-probe -- messages <channel-id>
KUBECORD_TOKEN=<token> cargo run -p kc-probe -- watch
KUBECORD_TOKEN=<token> cargo run -p kc-probe -- voice <guild-id> <channel-id>
KUBECORD_TOKEN=<token> cargo run -p kc-probe -- voice <guild-id> <channel-id> --stream --screen
```

`voice` joins a channel and speaks and receives; with `--stream --screen` it
starts a Go Live screen share through the XDG desktop portal. Add `--no-audio`
to test the voice handshake without opening the microphone or speaker.

## License

Kubecord is free software, licensed under the
[GNU General Public License v3.0 only](LICENSE).
