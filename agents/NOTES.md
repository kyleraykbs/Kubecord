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

## Verified against Lambda Ignition (2026-11-17)

- Gateway `cache` on the test guild: 4 channels in READY, including text
  `general` and voice `General`.
- REST history for text `general`: returns messages.
- Voice handshake for voice `General`: full endpoint, DAVE v1, key package and
  external sender; connection stayed up. Audio receive needs a second
  participant.

## Camera / Go Live fixes from live test (2026-11-17)

- Joining voice must not send `self_video: true` by default. Discord shows a
  camera tile for the flag and then errors 2015 when no track follows. The join
  now announces `voice.camera` (false unless the user toggled it); camera toggles
  send a main-gateway op 4 update with the new `self_video` value, in addition
  to voice op 12.
- Go Live needs `STREAM_SET_PAUSED { paused: false }` immediately after
  `STREAM_CREATE`, as DVS's `signalStream` does. Without it the stream is
  announced (viewers see LIVE) but never starts, then errors 2012.
- Keep the camera portal fd alive with the pipeline; dropping it made the
  camera source produce no frames.
- Stream/camera diagnostics now log stream voice READY (SSRCs, mode, DAVE),
  first encoded stream frame, and DAVE readiness.

## Join UI stuck on "Join Voice" (2026-11-17)

- Discord sends a self `VOICE_STATE_UPDATE` with `channel_id: null` for the
  previous state while a join is pending. The account loop used to treat that
  as a real leave, clear `pending_join`, and never connect; the user appeared in
  the channel but the UI stayed on Join Voice and no microphone path started.
  Now a null self state tears down only when a voice connection already exists.
- Guild `VOICE_STATE_UPDATE` carries a `member.user`; the reducer indexes it so
  voice-channel occupants have names even when they were not in READY.

## DAVE roster race (2026-11-17)

- With a second user already in the channel, the gateway sent an MLS add
  proposal before `CLIENT_CONNECT`; passing a local expected-user roster to
  `davey` rejected it (`unexpected user in add proposal`), closing the voice
  connection. Proposals now pass `None`: the voice gateway is the authenticated
  MLS delivery service. Live test with a second participant reached `DAVE ready`.
- `READY_SUPPLEMENTAL` voice states and guild `voice_states` now index
  `member.user`; unknown occupants fall back to their snowflake so the channel
  list does not silently drop them.

## Voice occupancy and camera pipeline (2026-11-18)

- Existing members in a voice channel are not delivered until the client sends
  gateway op 14 `GUILD_SUBSCRIPTIONS` for the channel. Opening a channel now
  subscribes with member ranges `[[0,99],[100,199]]`; live cache output then
  listed `voice General: kyleraykbs, Lambda Ignition`. This also feeds the
  member list.
- Camera/screen capture pipelines now include `videorate` before the
  width/height/framerate capsfilter; pipewiresrc can emit 0/1 or 60 fps and the
  old caps negotiation failed with "Element failed to change its state".
- Pipeline state failures now surface the GStreamer error and debug string in
  the UI status line.

## H.264 profile was the video failure (2026-11-18)

- The capture pipeline negotiated `High 4:4:4 Predictive` because nothing forced
  chroma layout; Discord expects 4:2:0 baseline (`42e01f`) for these streams.
  Constraining the capsfilter to `format=I420` makes x264enc emit
  `Constrained Baseline` level 3.1 (verified with ffprobe). This is the likely
  cause of the camera 2015 and Go Live 2012 timeouts.
- `videorate` precedes the caps so 0/1 or 60 fps sources can produce the
  requested 30 fps without failing state negotiation.

## pipewiresrc portal fd variant (2026-11-18)

- `pipewiresrc path=<node> fd=<fd>` failed state setup on the camera portal with
  `error set output format: -22`. `pipewiresrc fd=<fd>` alone works (verified
  90 encoded frames in 3s). The capture wrapper tries fd-only first and falls
  back to path/target-object variants.
- Failed pipeline attempts now go to NULL before being dropped, avoiding the
  "dispose element in READY" GStreamer critical.

## Screen capture selected the camera node (2026-11-18)

- `pipewiresrc fd=<fd>` without `path` defaults to the first portal node, which
  was the camera; Go Live therefore showed the camera feed instead of the
  selected display. Screen capture now prefers `path=<node>` and sets `fd`/`path`
  as element properties directly, matching the gtk4 paintable example. Camera
  capture keeps fd-only first because path fails its format setup.

## Screen fallback stole the camera (2026-11-18)

- Screen capture previously fell back to `pipewiresrc fd=<fd>`, which chooses
  the portal's first node (the camera). That made Go Live show the camera and
  left the camera unavailable to the camera toggle. Screens now try only
  `path=<node>` and `target-object=<node>`; fd-only is camera-only.
- Screen path probe: `variant=Path`, 2 frames in 3s on a static desktop; camera
  probe: `variant=FdOnly`, 90 frames in 3s.

## Streaming hitches (2026-11-18)

- `Srtp::decrypt_rtcp` treated the top bit of the trailing nonce counter as an
  RFC 3711 E-flag and returned feedback packets undecrypted. Every field past
  the 8-byte RTCP header was ciphertext, so NACK retransmission, PLI and RR
  stats were all dead. Discord's SRTCP is always AEAD with the counter nonce.
- RTCP datagrams are compound (RR + NACK, NACK video + NACK audio); walk them
  by length field.
- x264enc's default 600 ms VBV made every IDR ~150 KB at 2.5 Mbit/s; the pacer
  sent it at ~38 Mbit/s and the SFU dropped runs of packets out of the burst.
  `vbv-buf-capacity=150` plus a rate-based pacer (2.5x measured bitrate, 5
  Mbit/s floor) fixed it.
- `force_keyframe`: send the upstream force-key-unit event through the
  pipeline; the appsink's static sink pad returned false.
- Video RTP step must be `90000 / fps`, not `frame_ms * 90` (33 ms truncation
  ran the clock 1% slow).
- REMB (PSFB fmt 15) reported a constant 4.2 Mbit/s for a 720p30 stream.

## READY payload with capabilities 4605 (2026-11-18)

Captured with `KYLECORD_DUMP_DISPATCH=<dir>` (writes `<EVENT>.json` per
dispatch; contains account data).

- User objects are deduplicated: `users` holds them; `private_channels` carry
  `recipient_ids`, `relationships` carry `user_id`, `merged_members[i]` (aligned
  with `guilds[i]`) carry `user_id`.
- `read_state` and `user_guild_settings` are `{entries, partial, version}`.
- Legacy `user_settings` (with `guild_folders`, `status`, `custom_status`) is
  still present because capability bit 9 is not set. Keep it that way: the
  protobuf form has no parser here.
- Presences are in READY_SUPPLEMENTAL `merged_presences.{friends,guilds}` with
  `user_id` and a flat `status`; PRESENCE_UPDATE uses `user.id` and may carry a
  partial user that must not overwrite the cached one.
- GUILD_MEMBER_LIST_UPDATE group rows inside `items` have no count; counts are
  in the top-level `groups` array.

## UI development aids

- `KYLECORD_SCREENSHOT=<png>` renders the window to a PNG after 7 s and quits.
- `KYLECORD_OPEN=<channel id>` opens that channel once the account is ready.
- GtkLabel cannot embed images, so text with custom emoji uses a read-only
  GtkTextView with child anchors; text buffers reject `<a>` markup, so links
  in such messages are underlined but not clickable yet.
- Ellipsised labels report their full text as natural width; the sidebar is
  wrapped in an `AdwClamp` to keep a fixed width.

## Media display (2026-11-18)

- "Own GIFs don't show" was really WebP: Discord's GIF picker now returns
  Klipy links (`/gifs/*` still accepts `provider=tenor`). A Klipy link
  unfurls as a `gifv` embed whose thumbnail is WebP and whose `video` is
  an MP4. GTK reads only PNG/JPEG/TIFF itself and the rest through
  GdkPixbuf, which has no WebP loader here (`GDK_PIXBUF_MODULE_FILE` points
  at librsvg's cache). `images.rs` decodes WebP with `image-webp`. The
  media proxy's `format=webp` video posters needed the same fix.
- `gifv` embeds are short MP4s that Discord plays muted and looping. They
  play inline via `player.rs`: the file is downloaded by the image cache
  (no GStreamer TLS needed), and the still stays up until `AsyncDone`.
- Discord's media proxy keeps GIFs as GIFs when given `width`/`height`.
  Animated GIF attachments and animated custom emoji still show their
  first frame.
- `kc-probe raw /api/path` prints any GET as raw JSON; use it to check real
  payload shapes before guessing.

## GTK/libadwaita layout gotchas (2026-11-18)

- `AdwClamp` centres its child, and between its tightening threshold
  (default 400) and its maximum it eases the child narrower than its
  allocation while asking for more natural width. A left-aligned card
  therefore drifts right. Use `widgets::max_width`, which sets threshold =
  maximum.
- Negative margins are not supported in GTK4: they log "adjusted size …
  must not decrease below min" on every measure. The hover toolbar is one
  overlay child placed with `Overlay::connect_get_child_position`.
- Don't change a widget's visibility from a scroll adjustment's
  `value-changed`/`changed` handler: those fire during size allocation
  ("Allocation height too small"). Defer to an idle.
- `GtkFlowBox` asks for room for `max_children_per_line` children, so with
  `halign: center` one child still sits at the start. Set the maximum to
  the child count.
- The sidebar is rebuilt after each click, so a widget-level double-click
  gesture never sees the second press. Voice channels time double-clicks
  in `AppState::voice_channel_clicked`.
- The app is single-instance: a second launch activates the first and
  exits. `KYLECORD_SCREENSHOT` runs are `NON_UNIQUE` so they work while the
  app is open.
- New source files must be tracked (`git add -N`) or the flake build,
  which only sees git-tracked files, fails with "file not found for
  module".
- The flake checks `cargo fmt` and clippy with `-D warnings`; both pass
  as of this date.

## Voice receive (2026-11-18)

- Right after READY the voice gateway describes everyone already in the
  channel: op 11 (user ids), op 5 (user id + audio SSRC), ops 18/20. They
  arrive while the handshake is still waiting for op 4. The handshake used
  to keep only the DAVE opcodes, so people present before we joined never
  got an SSRC mapping and all their audio was dropped. Everything except
  heartbeat acks and HELLO is now replayed after the handshake.
- Op 5 is an SSRC announcement, not a per-utterance signal. Speaking is
  derived from voice packets arriving (`SPEAKING_HOLD` after the last one);
  clients stop sending when their user stops talking, after a few
  `F8 FF FE` silence frames that bypass DAVE.
- RTP and RTCP share the socket. RTCP is second byte 192..=223 (RFC 5761);
  `>= 200` misreads marker-bit RTP (Opus 248, H.264 229).
- Playback is pulled by the sound device (`appsrc need-data` into
  `pulsesink buffer-time=60000`), not pushed by a timer, and each speaker
  has a jitter buffer in `kc-media/src/receive.rs`.
- `kc-probe --token x playback-test` plays a synthetic speaker through the
  receive path; point `KYLECORD_PLAYBACK_DEVICE` / `KYLECORD_MONITOR_DEVICE`
  at a `module-null-sink` to run it silently.
- The microphone still transmits continuously while unmuted (no voice
  activity gating); the local ring comes from a level detector only.

## Video receive: cameras and watching streams (2026-11-18)

- The server sends no video until the client has sent its own video state
  (voice op 12, even "off") and media sink wants (op 15, `{"any": 100}`).
  Both go out right after the session description.
- Op 12 from the server maps a user to video SSRCs: each `streams[]` entry
  has `ssrc` and `rtx_ssrc`. `video_ssrc: 0` or no active stream means the
  video stopped. Retransmissions arrive on `rtx_ssrc` (payload type 102)
  with the original sequence number in the first two bytes.
- Receive path: `kc-voice/src/video_rx.rs` reorders packets, asks for
  missing ones (NACK, `rtcp.rs`), waits up to 250 ms for a retransmission,
  then drops the frame and holds everything until a keyframe (requested
  with PLI). Whole frames are DAVE-decrypted, then decoded by
  `kc-media/src/decode.rs` (`avdec_h264`), shown through
  `gtk4paintablesink` in `kc-ui/src/live.rs`.
- RTCP we send is a compound packet (empty receiver report + feedback),
  encrypted like RTP: first 8 bytes clear and authenticated, 4-byte nonce
  counter appended. Verified live: PLI and NACK are honoured.
- A frame is several NAL units (SPS, PPS, fragmented IDR). Only the marker
  bit or a new timestamp ends it; DAVE decrypts the frame as a whole.
- Watching a stream: gateway op 20 with `guild:{guild}:{channel}:{user}`
  (or `call:{channel}:{user}`), then STREAM_CREATE (`rtc_server_id`) and
  STREAM_SERVER_UPDATE (token, endpoint), then a second voice connection
  with `server_id = rtc_server_id`, DAVE group `rtc_server_id - 1`, and
  nothing transmitted (`VoiceParams::receive_only`). Op 19 leaves it.
- Checks without a second person: `kc-probe --token x video-test [N]`
  (offline, loses every Nth packet), `KYLECORD_VIDEO_DEMO=1` (test pattern
  in the stream view), and `kc-probe watch-stream <guild> <channel> <user>`
  (headless, against a real stream).
- Not done: stream sound is routed to playback but has not been heard
  live; video is shown as it arrives (no clock smoothing); software decode
  only; no fullscreen or pop-out; own camera has no local preview.


## UI parity additions (2026-11-18, later)

- Right-click user menu (`menu.rs`): attached to member rows, voice tiles,
  sidebar voice rows, DM rows and message avatars. Popovers are parented to
  the window via `anchor_popover` (not the clicked row) so a sidebar rebuild
  under an open menu can't orphan the grab and freeze input.
- Profiles are a floating `adw::Dialog` card (members.rs `show_profile`),
  with Member Since, mutual counts, editable note, and — for yourself — an
  Edit Profile button (`edit_profile_dialog`: global name, pronouns, bio via
  `set_global_name` / `set_profile`).
- DM/group-DM voice calls: header call button (`toggle_dm_call`), `join_call`
  rings via `ring_call`. Screenshare and camera now work in DM calls —
  `stream_create` takes `Option<guild>` and emits a `call` stream; the
  guild-only guards are gone. `stream_key(guild, channel, owner)` builds the
  key for both guild and `call:` forms.
- Incoming DM call: `CALL_CREATE` with us in `ringing` → `UiEvent::IncomingCall`
  → a toast with a Join button.
- Per-user volume: `Command::SetUserVolume` → account loop sets it on the
  voice audio (and the stream-audio key); remembered in `AppState.user_volumes`
  and re-applied on reconnect.
- Audio device selection in Settings (the former "Capture Quality" dialog,
  now "Settings"): `kc_media::devices` enumerates sources/sinks and holds the
  process-global choice; `CapturePreferences.{input,output}_device` persist it
  and `apply_devices()` pushes it to capture/playback. Both default to system
  default. Takes effect on the next call.
- Server header is a dropdown (`server.rs`): Invite, Create Channel/Category,
  Server Settings (rename), Mute/Unmute, Copy ID, Leave — gated by perms.

## Settings panel and user bar (2026-11-18)

- Settings is a two-pane `adw::Dialog` (`kc-ui/src/settings_panel.rs`): My Account and Voice & Video. Changes apply immediately via `AppState::update_prefs`, which saves to `settings.json` after a 400 ms debounce.
- Mic/speaker volume are process-global gains in `kc_media::devices` (`set_input_gain`/`set_output_gain`), applied live in the capture callback and the playback mixer. `audio::MicTest` loops the mic back to the chosen output and reports the peak level.
- Camera/screenshare tests run the real encode pipeline into a `VideoFeed` and decode it with `LiveVideo`, so they show what viewers get at the current quality settings. Use `spawn_local` there, not `AppState::spawn`: its callback must be `Send`, and GTK objects are not.
- Audio devices are listed via `pulsedeviceprovider`; a general `DeviceMonitor` also probes cameras and floods the log with GstFractionRange criticals.
- The channel sidebar is a `gtk::Paned` (default 340px, 220–480, persisted as `sidebar_width`). The settings gear sits in the user bar beside Deafen.
- Dev aid: `KYLECORD_SETTINGS=account|voice` opens the panel 4 s after launch (pair with `KYLECORD_SCREENSHOT`).

## Camera and Go Live popouts (2026-11-18)

- The camera and screen buttons always open a popout (`kc-ui/src/video_setup.rs`) before anything is sent: "Ready to video chat?" and "Share Your Screen". Both offer an optional preview, the source, and one-time quality and Advanced settings (a copy of the prefs; Settings holds the permanent values). The share popout adds source type (screen/window/camera), Go Live delay, and a system-audio switch (`StartStream { audio }`).
- A previewed screen goes live without a second picker by handing over the running capture, not by reconnecting. The preview's `VideoSource` sends frames through a `FrameTap`. Go Live passes `PreparedCapture { source, tap }` in `Command::StartStream { capture }`, and the session points the tap at its sender and forces a keyframe. Reconnecting to the portal node does not work on every portal: the node vanished after the preview disconnected, and pipewiresrc fell back to the default camera. Screen captures now set `node.dont-fallback`/`node.dont-reconnect` in `stream-properties`. A quality change stops a screen preview, which can't be reopened without the picker.
- `quality_controls(initial, stream, apply)` builds the quality rows for both Settings (`apply` = `update_prefs`) and the popouts (`apply` = their local copy).
- Clicking outside the visible `adw::Dialog` closes it (capture-phase `GestureClick` on the window in `connect_signals`).
- A wrapping label in a horizontal box needs `max_width_chars(1)` + hexpand, otherwise its height is measured at full width and GTK logs "Allocation height too small".
- Dev aid: `KYLECORD_POPUP=camera|share|profile` opens that popout 4 s after launch.

## Profile popout (2026-11-18)

- `kc-ui/src/profile.rs` replaces the old card in members.rs; `show_profile(user_id, anchor)` keeps its signature. Left: a banner (image, else the theme/accent colour, else a colour picked from the user id) with the avatar over it; name, original names, the actions, bio (rendered with `rich`), member/friends since, roles, note. Right: Activity / Mutual Friends / Mutual Servers in an `adw::InlineViewSwitcher`.
- Server/Global (`adw::ToggleGroup`) shows only when they are a member of the current server: cached, or the profile response has `guild_member`. Server look = `guild_member_profile` / `guild_member`, each field falling back to the global one.
- The name on top is server nick > friend nick > global name; the username line is always shown, plus the global name when it differs.
- Nicknames (pencil button): friend nickname via `PATCH /users/@me/relationships/{id} {nickname}` (`set_friend_nickname`); server nickname needs CHANGE_NICKNAME (self) or MANAGE_NICKNAMES plus a higher top role (others). Owner and administrator can always.
- Widgets hold the `Profile` weakly; the dialog's `closed` handler holds the only strong ref, so a closed profile is freed. Rebuilds after a signal go through `idle_add_local_once`, because the card owns the widget emitting it.
- Dev aids: `KYLECORD_POPUP=profile|profile-guild|profile-friend` (own; own inside the first server; first friend inside the first server).

## Captchas (2026-11-18)

- Friend requests can get a captcha challenge: a 400 whose body is `{"captcha_key": [...], "captcha_sitekey", "captcha_service": "hcaptcha", "captcha_rqdata", "captcha_rqtoken"}`, with no `message`/`code`. Confirmed with `kc-probe befriend <username>` (PUT relationships). kc-http turns it into `Error::Captcha`; other non-standard error bodies now show up in the message instead of being dropped.
- With the `captcha` feature (kc-ui; on by default in the kylecord binary and the Nix package, which adds webkitgtk_6_0 + glib-networking), `AppState::with_captcha(prompt, request, done)` runs a request. On `Error::Captcha` it shows the hCaptcha in a WebKit view and retries with `X-Captcha-Key`/`X-Captcha-Rqtoken` (`kc_http::CaptchaAnswer`). The page is loaded via `load_html(.., base_uri = "https://discord.com/")`, so hCaptcha's site check passes: Discord's real key `a9b5fb07-…` renders a normal checkbox there. The view uses an ephemeral network session and the client's own user agent. Confirmed working end to end on a live friend-request challenge (2026-11-18).
- Off (Settings → Compatibility), closed unsolved, or built without the feature: the caller gets the challenge error and shows `captcha_needed` (a toast with Open in Browser). The test button and `KYLECORD_POPUP=captcha` use hCaptcha's public test key. `KYLECORD_SCREENSHOT_DELAY` waits longer for network pages.
- kc-probe takes the token from `KYLECORD_TOKEN`. Set it from `~/.config/kylecord/accounts.json` with jq, without printing it.

## DM calls in the chat column (2026-11-18)

- The chat column is a vertical `gtk::Paned`: `call_area` (the voice tiles) above `chat_box` (messages + composer). `voice::call_layout` decides: voice-only channel → `Only`; a DM whose call has anyone in it (you or not) → `WithChat`; otherwise `Hidden`. `render_messages`, `refresh_voice_view` and `render_voice_view` all go through it, so hanging up or the last person leaving takes the call away. The height is saved as `call_height`.
- Discord only sends DM-call voice state updates to the people in the call. The store seeds participants from `CALL_CREATE.voice_states` and clears the channel's voice states on `CALL_DELETE`, which arrives when the call ends.
- A DM call you are not in shows a Join Call button under the tiles. Dev aids: `KYLECORD_POPUP=call-dm` / `call-dm-end` (a local fake CALL_CREATE, then CALL_DELETE).
