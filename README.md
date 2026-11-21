# Kubecord

A Discord client for Linux, built from scratch in Rust!

The Linux community has been shackled by the official Discord client for far too long! I thought someone else would do it, but I guess if you want something done right (or done at all) you gotta do it yourself. Welcome to your liberation from electron! Please make issues if you have suggestions for features or find any bugs, I've tested the client extensively but I'm sure I missed *something*.

<img width="1436" height="815" alt="image" src="https://github.com/user-attachments/assets/42b06599-ad76-43e1-a947-bfdaa6c2c8fc" />


## Warning
Third-party clients are *kinda* (see below for more information) against Discord's TOS 
and *can* get the account banned. Though the risk is quite low as discord mainly tries
to detect 'self-botting' and mass friending for scam prevention reasons.

This client does it's best to adhere to the proper usage of the API. See below for more details.
<details>
<summary>Click to expand</summary>

The specific lines in the Discord TOS that this *may* violate and a description of how:

**"using any unauthorized software designed to modify the services"**
```
While there technically are modifications to the original intent they are minor
many other projects like Vesktop and Vencord allow for plugin modifications that
do much more. Those clients and modifications are generally considered low risk.

This project is slightly more risky as we are attempting to recreate the API
interaction in full rather than just tacking onto the existing app.

Our modifications are basic, and attempt to be respectful, to get an idea here's some:
- Show people if you're typing or not
- See stream details
- Search GIF favorites 
```

**"scraping our services without our written consent, including by using any robot, spider, crawler, scraper, or other automatic device, process, or software"**
```
Our client doesn't attempt to automate any scraping or messaging. It simply allows for
the *user* to interact with Discord just like they normally would. 
```

**"copying, dismantling, or reverse engineering any of our services or using our intellectual property without permission"**
```
You aren't reverse engineering anything while using this client, although we
did use *other peoples* clients as a reference, and reverse engineering was
required in the making of them. We never reverse engineered DiscordApp itself.

Clients like Vesktop and Vencord also had done the same.
```

**"trying to gain access to, intentionally overburdening, or attacking our systems"**
```
Our client isn't attempting to bypass any rate limits or flood discords servers, although if something isn't impedimented yet on the clients end then it *could* appear like that on discords end, say if you are trying to spam and there is a rate limit I haven't yet implemented.
```

In summary, we aren't doing anything malicious and that's where the main ban risk is. Discord only tends to ban from MASS calls or really suspect API behavior, our client tries to be respectful to the API but I can't ensure that all calls won't get flagged. From my own use however, it seems very unlikely. Be respectful and a ban is unlikely. Friending people is the most likely place to get a ban.
</details>


## Features
### 1 - Voice call support
- Send/Receive audio
    - See who is talking
    - Control individual audio volume
    - Audio volume boost up to 5x

<img width="653" height="371" alt="image" src="https://github.com/user-attachments/assets/e46c6fed-7ef3-4d15-b2d2-158508b4c13e" />

- Screenshare & Webcam
    - Audio support!!

      <img width="360" height="159" alt="image" src="https://github.com/user-attachments/assets/9b550203-9f33-472f-8f1e-3b2de301846b" />

      - Whole desktop
      - Specific window
      - Dummy node for any patchbay
    - Debug details
    - Screen-share preview shows the *encoded* video stream, what you see is what others see. (Barring connection stutters)
    - Ability to see the debug data of screen-shares *you* are watching.
    - MANY fine-tunable settings (you're in full control)
- DM call support
    - Shows when ringing the other party
    - Group chat support
- Noise suppression
- Echo cancellation
- Sensitivity

<img width="420" height="376" alt="image" src="https://github.com/user-attachments/assets/74427ba5-3679-47b1-b78c-f0f7fddfb376" />


### 2 - Text channel support
- Receive and send texts
- Media
  - Images
  - Video
  - Embeds
  - Attachments (with download)
- Gifs
    - All types, MP4, GIF, etc.
    - Favorites
    - Search (Including partial favorites search, matched by link)
- Emoji (Server & Standard)
- Stickers (all except the stock discord stickers)
- Spellchecker
- Shows typing
    - Ability to hide your own typing
    - Shows both when *you* and other people are typing
- All available admin tools
- Edit admin channel settings

### 3 - Profiles
- Edit your own profile
- View other peoples profiles
- Status, Notes & Bio
- Mutual friends
- Mutual servers
- Basic activity support

### 4 - Whole app
- Not built in electron! (Biggest feature of them all)
- Built in GTK
- Notifications
- Login
  - Token
  - Username & Password
  - 2FA
  - SMS verification
- Captcha support
- Follows all GTK colors (Stylix compatible)
- Full admin compatibility
- Extensive settings

## Unsupported
- In call sound effects
- Discord web games like it's golf game, chess, wordle, etc
- Animated profile overlays

## Technical Details
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
libopus and the Rust toolchain. The packaged binary wraps
`GST_PLUGIN_SYSTEM_PATH_1_0` and the GTK schemas.

## Logging in

Add an account from the login page with either an **email and password** (with a
two-factor code when the account requires one) or a **user token** directly.
Discord may ask for a captcha or an email confirmation for a login from a
third-party client. Only the resulting token is kept; the password and any codes
are never stored or logged. The token is stored in
`$XDG_CONFIG_HOME/kubecord/accounts.json` with mode 0600. Multiple accounts are
supported; each account owns its gateway connection, REST client and at most one
voice connection.

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

## Layout

```
crates/kc-core      snowflakes, accounts, tokens, super-properties
crates/kc-http      REST v9, rate limits, uploads
crates/kc-gateway   user gateway, heartbeat, identify/resume
crates/kc-state     Discord models and the event reducer
crates/kc-voice     voice gateway, UDP/SRTP/RTP, DAVE, streams
crates/kc-media     GStreamer capture, Opus, H.264, rendering
crates/kc-ui        GTK4/libadwaita application
crates/kc-probe     protocol probe CLI
crates/kubecord     application entry point
```
