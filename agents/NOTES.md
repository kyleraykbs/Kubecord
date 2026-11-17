# Notes

## Protocol facts that cost exploration

- User tokens cannot use twilight or serenity: both force the `Bot ` prefix
  (`twilight-gateway` `ConfigBuilder::new`). The gateway and REST are ours.
- Discord's user gateway is v9 JSON. IDENTIFY carries `capabilities`,
  `client_state.guild_versions`, `properties` (super-properties) and `presence`;
  it does **not** carry `intents` or `shard` for this client shape.
- DAVE is mandatory for voice since 2026-03-02; a client without it is closed
  with 4017. The voice gateway reports `dave_protocol_version` in op 4; MLS
  messages are binary opcodes 25–31 with a 2-byte sequence prefix on receive.
- DAVE group id is the channel id for a voice connection, and
  `rtc_server_id - 1` for a Go Live stream connection.
- Voice transport can be UDP (`select_protocol: "udp"`, SRTP rtpsize modes) or
  WebRTC. UDP is the documented primary path and carries audio, camera and
  streams; it avoids per-SSRC transceiver renegotiation. Chosen: UDP.
- SRTP rtpsize: encrypt payload only; AAD is the RTP header (12 bytes plus the
  4-byte extension preamble when X is set); append the 16-byte tag, then the
  4-byte nonce counter. Nonce buffer is 12 bytes (AES-GCM) or 24 (XChaCha), the
  counter in the first four bytes, the rest zero.
- Go Live is a second voice connection, not a mode of the parent. Main gateway
  op 18 creates, op 19 deletes, op 20 watches, op 22 pauses; the stream server
  details arrive as `STREAM_CREATE` and `STREAM_SERVER_UPDATE` keyed by
  `stream_key = guild:<guild>:<channel>:<user>` (or `call:<channel>:<user>`).
- Voice heartbeat is op 3 JSON `{t, seq_ack}` with `seq_ack` from the last
  binary opcode sequence number.
- The UGREEN `2b89:5389` capture card is single-claimant. Never open
  `/dev/video0` while OBS may be using it; camera capture goes through the
  XDG camera portal and the code excludes UGREEN/MACROSILICON nodes.

## Environment gotchas

- `reqwest` 0.12 with `rustls-tls-webpki-roots`, not 0.13: 0.13's `rustls`
  feature always uses `rustls-platform-verifier` and fails in the Nix build
  sandbox with "No CA certificates were loaded from the system".
- The `opus` crate builds Opus with CMake; use `opus2` (links the system
  libopus through pkg-config) instead.
- `ashpd`'s `pipewire` feature pulls the `pipewire` crate, which runs bindgen:
  the build needs `clang` and `LIBCLANG_PATH`.
- GStreamer plugin paths are not implicit in `nix develop`; the flake shell
  exports `GST_PLUGIN_SYSTEM_PATH_1_0`, and the packaged binary wraps it.

## Abaddon parity findings (2026-11-17)

Reference: `reference-projects/abaddon` (uowuo/abaddon, commit 7b3a4ff).

- **READY guilds carry channels.** Abaddon calls `ProcessNewGuild` for every
  guild in READY, not only GUILD_CREATE, and attaches the enclosing guild id to
  each embedded channel. Our reducer previously inserted them with
  `guild_id: None`, so `guild_channels()` returned nothing and the sidebar
  said "No channels". Fixed in `State::merge_guild_channels`; GUILD_CREATE is
  now also covered. Regression tests are in `kc-state`.
- **Web identity, not desktop.** Abaddon sends `browser: "Chrome"`, no
  `client_version`, capabilities `4605`, and `client_state` with
  `guild_hashes` / `highest_last_message_id` / read-state versions. Our old
  `client_state: {guild_versions:{}}` and `capabilities: 0` were the wrong
  shape. Live Lambda test: READY now returns the test guild with four channels.
- **Voice endpoint includes a port.** Use the full `VOICE_SERVER_UPDATE`
  endpoint (`host:port`) in `wss://.../?v=9`; stripping the port caused a 4006
  "Session is no longer valid".
- **Arm media after the session description.** Abaddon sends a speaking payload
  with `speaking: 1` (microphone) and one encrypted Opus silence frame
  `F8 FF FE`, then starts the UDP reader. Sending only `speaking: 0` is not
  equivalent.
- **RTCP is not RTP.** The RTCP packet type is the full second byte
  (200–206); masking the RTP marker bit turns 201 into 73 and tries to SRTP
  decrypt receiver reports every second. Classify RTCP before RTP parsing.
- **RTP header extensions.** SRTP `rtpsize` authenticates only the header and
  extension preamble, but the extension block itself is inside the encrypted
  region. The codec payload starts after the full extension block
  (Abaddon's `GetPayloadOffset`); `RtpHeader::payload_offset` now carries that.
- **SOUNDSHARE vs microphone.** Stream audio must be sent with speaking flag
  `2`; `VoiceHandle::send_soundshare_audio` does that, while the parent voice
  connection keeps flag `1`.
- Abaddon itself does not send camera or Go Live video (voiceclient only handles
  Opus); its value is the gateway/REST/voice state machine, not video.
