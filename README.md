# kylecord

A Discord client for Linux, built from scratch in Rust.

The UI is GTK4/libadwaita and inherits the system Adwaita colours, so it follows
stylix. Media is GStreamer. Voice uses Discord's UDP voice protocol directly:
SRTP with `aead_xchacha20_poly1305_rtpsize` (or AES-256-GCM when offered) and
DAVE end-to-end encryption through the `davey` crate. There is no Electron, no
webview and no patched official client.

## Build and run

```sh
nix develop
cargo run -p kylecord
```

or, from the flake:

```sh
nix run .
```

The flake provides GTK4, libadwaita, GStreamer with the plugins the client uses,
libopus and the Rust toolchain. The packaged binary wraps
`GST_PLUGIN_SYSTEM_PATH_1_0` and the GTK schemas.

## Logging in

Add an account from the login page with a **user token**. The token is stored in
`$XDG_CONFIG_HOME/kylecord/accounts.json` with mode 0600. Multiple accounts are
supported; each account owns its gateway connection, REST client and at most one
voice connection.

`kc-probe` is the protocol probe used during development:

```sh
KYLECORD_TOKEN=<token> cargo run -p kc-probe -- user
KYLECORD_TOKEN=<token> cargo run -p kc-probe -- guilds
KYLECORD_TOKEN=<token> cargo run -p kc-probe -- channels <guild-id>
KYLECORD_TOKEN=<token> cargo run -p kc-probe -- messages <channel-id>
KYLECORD_TOKEN=<token> cargo run -p kc-probe -- watch
KYLECORD_TOKEN=<token> cargo run -p kc-probe -- voice <guild-id> <channel-id>
KYLECORD_TOKEN=<token> cargo run -p kc-probe -- voice <guild-id> <channel-id> --stream --screen
```

`kc-probe voice` joins a channel, speaks/receives, and with `--stream --screen`
starts a Go Live screen share through the XDG desktop portal.

## Warning

Using a user token with a third-party client is against Discord's terms and can
get the account banned. That risk is inherent to the project, not a bug.

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
crates/kylecord     application entry point
```

`reference-projects/` holds cloned reverse-engineering projects used as
protocol references; it is gitignored and not part of the build.