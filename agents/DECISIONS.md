# Decisions

Format: `## [date] title`, then Decision / Why / Reversible.

## [2026-11-12] Rust + GTK4/libadwaita + GStreamer

- Decision: single Rust workspace; GTK4/libadwaita UI; GStreamer for capture, codecs and rendering.
- Why: gtk4-rs and gstreamer-rs are first-class; `davey` is a pure-Rust DAVE implementation; user prefers GTK and default colors for stylix.
- Reversible: no.

## [2026-11-12] Own gateway and REST instead of twilight/serenity

- Decision: implement the user gateway (`/gateway`, JSON, v9) and REST client in-tree.
- Why: twilight and serenity unconditionally prefix tokens with `Bot `, so they cannot authenticate a user account. The client also needs full control over super-properties/config.
- Reversible: yes, with a fork.

## [2026-11-12] UDP + SRTP as the only media transport

- Decision: voice, camera and Go Live all use the UDP voice path (`select_protocol: "udp"`), SRTP with `aead_xchacha20_poly1305_rtpsize`, and DAVE frame transforms.
- Why: Discord documents WebRTC as the fallback for clients that cannot do UDP; stream connections use the same protocol as voice. UDP avoids WebRTC's per-remote-SSRC transceiver renegotiation and SDP synthesis.
- Reversible: yes — `kc-voice` gets a `Transport` trait; Discord-video-stream is the WebRTC blueprint.

## [2026-11-12] Multi-account from the start

- Decision: every account owns a gateway task, REST client and at most one voice connection; the UI switches the active account.
- Why: user requirement. Retrofitting per-account state into a single-account event pipeline is expensive.
- Reversible: no.

## [2026-11-12] Plaintext token store, 0600

- Decision: `$XDG_CONFIG_HOME/kylecord/accounts.json`, mode 0600, no keyring dependency.
- Why: single-user local client; a keyring adds a daemon requirement to a GTK app.
- Reversible: yes.