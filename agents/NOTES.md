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

## Webcam framerate and self-preview (2026-11-18)

- Webcams must be captured as MJPEG: a C920 gives raw 720p at 10 fps (USB 2 bandwidth) but MJPEG 720p at 30. The old `video/x-raw` capsfilter got the 10 fps mode and `videorate` padded it with duplicates sent three at a time, which viewers saw as stutter. `camera_inputs` lists what to ask for, best first (MJPEG exact, larger MJPEG, MJPEG any rate, raw exact, raw ≥640 wide, any raw); every entry but the last must produce a frame (`wait_for_first_frame`, up to 15 s). A format the node lacks is refused at once ("no more input formats"), so silence means a slow camera, not a wrong format; one silent format skips to the last resort. `VideoSettings.camera_format` (pref `camera_format`: 0 auto, 1 MJPEG, 2 raw) narrows the list.
- `kc-probe camera-timing [seconds] [name part] [cpu|auto] [kbit/s]` runs a camera through the real pipeline and prints frame gaps, fps and kbit per second. `RUST_LOG=kc_media=info` shows the format chosen. The C310 (046d:081b) takes about 8-10 s to deliver its first frame. In the probe its frames then arrive in bursts (about 85 real frames every ~3 s, nothing between) in both MJPEG and raw, while the C920 is even; where they are held is not found yet. Camera pipelines use `videorate skip-to-first=true`, or the warm-up time is filled with a burst of duplicates.
- Self-preview: `LocalPreview` (session.rs) sits in the camera and stream frame callbacks, pushes the encoded frames into a `VideoFeed` and reports them as `VideoStarted`/`VideoStopped` for your own user id, so the tiles show them like anyone's video. It starts on a keyframe and ends when the capture is dropped. `set_self_preview` (pref `self_preview`, the tile menu checkbox, Settings → Voice & Video) gates it live. Not yet seen in a real call.

## Camera frame damage, colour range, call log (2026-11-18)

- A webcam's MJPEG frame that loses a USB packet still arrives, shorter; `jpegdec` decodes it with garbage and gray from the hole on (a libjpeg warning, not an error). `kc_media::mjpeg::check` walks the entropy-coded data (Huffman codes, 64-coefficient blocks, end marker right after the last block) and a pad probe on `jpegdec name=mjpeg` (`guard_mjpeg`) drops damaged frames; `videorate` repeats the previous one. Frames it cannot walk (progressive, no DHT of their own) pass unchecked; if most of the last 60 frames fail, the check switches itself off.
- PipeWire hands camera frames over in full-size buffers (1,843,200 bytes for 720p) with stale data after the JPEG, so "ends with FFD9" is not a usable truncation test.
- MJPEG decodes to full-range YUV. The encoder input caps now carry `colorimetry=bt709` (bt601 below 720p), so `videoconvert` converts to limited range; before, Discord's decoder stretched the full-range picture and viewers saw it darker with crushed shadows.
- `~/.cache/kylecord/kylecord.log` (previous run in `.log.1`, mode 0600) records info-level logs of every crate. kc-voice logs a "sent video" line every 5 s while sending: frames, kbit, dropped, nack_requested/nack_found, keyframe_requests, backlog_max_ms, loss_max_percent, remb_kbit. Far-end camera freezes that do not show in the self-preview are still undiagnosed; read this log after a call that had them.

## Ping and the first call log (2026-11-18)

- Ping: the time from a voice gateway heartbeat (op 3, sent every 5 s, more often than the server's interval) to its acknowledgement (op 6). The server does not echo an eight-byte counter keepalive over UDP (tried: no replies), so the UDP keepalive is back to Abaddon's two bytes every 10 s. `VoiceEvent::Ping { millis }` (None when a heartbeat went unacknowledged) → `UiEvent::VoicePing` → `kc-ui/src/ping.rs`: signal bars in the voice panel and a five-minute graph in its popover. Dev aid: `KYLECORD_POPUP=ping` (made-up samples).
- First logged camera call (130 s, 1 Mbit/s, C920 MJPEG): 150 frames every 5 s, no drops, backlog under 30 ms, no keyframe requests after the first, REMB 9 Mbit/s. Six bursts of video NACKs (1, 2, 6, 19 with 4% reported loss, 8, 4), all answered from the retransmit cache. So the far-end hitches line up with short packet-loss bursts on the uplink, repaired by retransmission; whether the server had to ask twice is now counted as `nack_repeated`.
- x264 with `tune=zerolatency` slices each frame per thread: about 13 RTP packets for a 4 KB frame. Not changed.

## Video retransmission must be RTX (2026-11-18)

- Hitch times the user noted matched "video packets asked for again" log lines to the second: 1-4 packets lost on the uplink, all found in the cache and resent, never asked for twice, and yet the viewer froze about 2 s (the keyframe interval) where the official client recovers in 0.25-0.5 s. The resend was a copy of the original packet on the main SSRC. Discord repairs video as RFC 4588 RTX: on the stream's `rtx_ssrc`, payload type 102, its own sequence numbers, the original sequence number in the first two payload bytes (the same form the server sends us). `SessionState::retransmission` now builds that from the cached pre-SRTP payload (`SentVideo`); audio is still resent as it was. A NACK for packets no longer cached forces a keyframe.
- `KYLECORD_TEST_DROP_VIDEO=n` skips sending every nth video packet, to test repair without waiting for real loss. Not yet confirmed against a live viewer.
- Forced-loss run (`KYLECORD_TEST_DROP_VIDEO=300`) after the RTX change: the server asked for each withheld packet once, every one was resent as RTX and almost never asked for twice, no viewer keyframe requests arrived, and the viewer still froze 0.5-2 s per loss (until the next scheduled keyframe). So neither a plain resend nor RTX has been seen to unfreeze an official-client viewer, and no PLI reaches us during those freezes. Every video NACK now also forces a keyframe (`VoiceEvent::ForceKeyframe`); `VideoSource::force_keyframe` spaces them 750 ms apart and defers, rather than drops, a request inside that time. Non-REMB picture feedback (RTCP 206) is logged at info with its media SSRC, to learn whether viewers' requests arrive under an SSRC we ignore.
- Forcing a keyframe on every video NACK backfired: from the first loss on, the server asked for about 29 single packets a second (RR loss 7-9%, roughly one keyframe's worth of packets per forced keyframe, 750 ms apart) where calls before had one small burst every ~10 s. Why forced keyframes are followed by resend requests is not understood. Loss-triggered keyframes are now at least 3 s apart (`LOSS_KEYFRAME_SPACING`). The NACK log line lists each packet as `sequence:index/count[K]@age` (place in its frame, K for a keyframe, time since first sent), and each keyframe sent is logged, to find out which packets the server asks for.

## Camera hitches follow scheduled keyframes (2026-11-18)

- With every keyframe logged, the hitch times the user noted did not match resend requests (3 in 75 s, at other times) but each fell 0.4-1.4 s after a scheduled keyframe, in two separate calls, and the hitches lasted about the keyframe interval (2 s). Reading: about one scheduled keyframe in six is not accepted by an official-client viewer, which then shows nothing new until the next one. Why is not known. The viewer sends no NACK or PLI in that time (libwebrtc waits 3 s before asking).
- Ruled out: the end-to-end encryption step. 3,003 recorded camera frames (`KYLECORD_DUMP_FRAMES=<file>` with `kc-probe camera-timing`) replayed through davey's Encryptor → WebRTC-style packetise/reassemble → Decryptor all decrypt, in order and with video delayed behind interleaved audio (the nonce counter is shared). `rtp::packetize` reproduces encrypted frames byte for byte (brute-force test in rtp.rs). Camera frames here are an AUD plus 11 slices, long start codes throughout, slices of 6-9 bytes in repeated frames.
- Cameras now send keyframes only on request (`keyframe_secs = 0` → one-hour GOP, set in `start_camera`), as WebRTC senders do: on PLI/FIR, and on video NACKs at most every 3 s. The Keyframe Interval setting applies to Go Live only.
- The user confirmed the camera is smooth with on-demand keyframes, and that Go Live had only looked smooth with a high stream delay. Go Live now defaults to on-demand too: `KEYFRAME_INTERVALS` gained 0 ("Only when needed", the default) and the pref is stored as `stream_keyframe_secs`, so the old default of 2 in existing settings files is ignored. A schedule can still be chosen (Scheduled Keyframes, under Advanced).
- Screenshares then stopped delivering frames: the stream connected, "stream source ready" was logged, and no frame ever came (viewers saw the tile appear and vanish). The user's compositor is custom ("europa"; its portal reports source type 4) and the screen `pipewiresrc` setup is known to be temperamental: what worked was found by trial. The only change to that source since it worked was `stream-properties` (`node.dont-fallback`, `node.dont-reconnect`), added for a bug since fixed another way (the preview capture is handed over, not reconnected). Removed: the screen source sets `fd` and `path` only. Do not add properties to it without testing a real share. Whether this restores screenshares is not yet confirmed.
- The capture thread is now the only reader of the pipeline bus: errors and warnings are logged ("capture pipeline error"), the latest error is kept for `wait_for_first_frame`, and a pipeline silent for 4 s logs once.

## Logging switches, defaults and guidance (2026-11-18)

- Logging lives in `kc-ui/src/logging.rs` (`kc_ui::logging::init()` from main): terminal as before; the file layer uses a dynamic filter and a writer capped at 20 MB per run. Prefs `log_to_file` (default on) and `log_detailed` (default off; adds debug from kc_voice, kc_media and kc_ui::session) apply live through `logging::configure`, called from `apply_devices`. Settings → Debug has the switches, this run's size and Open Folder. Normal logging measured 20 KB for a 7-minute session with several streams (about 170 KB/hour). `RUST_LOG`, when set, overrides both outputs.
- Defaults reviewed: Go Live delay 1 s (the user's choice: it evens out uneven capture; each loss-recovery keyframe waits that long in the delay queue), automatic bitrate lowered to 0.07 × pixels × fps, clamped 800-6000 kbit/s (1.9 Mbit/s at 720p30, 4.4 at 1080p30; the server's REMB for a stream was 4.2), keyframes on demand in `VideoSettings::default` too. Every quality row carries a subtitle saying what it trades and what to leave alone.

## Camera plus Go Live together (2026-11-18)

- With a 1080p stream (about 4.1 Mbit/s) and a camera (about 1.9 Mbit/s) both running, resend requests rose sharply (stream: up to 55 packets per 5 s, 2-4% reported loss; camera: every few seconds), where either alone had a small burst every 7-10 s. That looks like the upload nearing its limit. There is no congestion response: bitrate never follows the receiver reports or REMB. An adaptive bitrate would be the next step.
- A long camera freeze traced to the loss-keyframe spacing: a loss within the spacing of the previous loss keyframe got none, and with scheduled keyframes gone nothing ended the freeze until the next loss. Such a loss now leaves a keyframe owed (`loss_keyframe_owed`), sent from the 100 ms tick as soon as the spacing (now 2 s) has passed.
- A tile with both a stream and a camera for the same person shows the camera as an inset in the stream's corner; clicking the inset focuses the camera.
- Unexplained: bursts of davey warnings "Encrypted frame is too small to contain min supplemental bytes" (30/s for several seconds) on the receive side, i.e. tiny frames arriving from the other participant.

## Bitrate adaptation, loss recovery spacing, dead buttons (2026-11-18)

- With Bitrate on Automatic (`VideoSettings.adaptive_bitrate`), the encoder follows the network: kc-voice emits `VoiceEvent::VideoNetwork { sent, lost, remb_kbps }` every 2 s (video packets sent, packets NACKed, the server's REMB); the session passes it to `VideoSource::network_report`, where `BitrateController` cuts 15% when a report shows 2% loss or more (floor: 30% of the target), adds 10% after five reports averaging under 0.8%, and stays under 95% of REMB. The encoder is `name=enc`; x264enc takes a new `bitrate` while running (tested: 3000 → 600 kbit/s). A fixed bitrate never adapts. Changes are logged as "video bitrate adapted". Not yet seen in a real call.
- The least time between loss keyframes is a setting (Fastest Loss Recovery, pref `loss_keyframe_ms`, default 1 s; `VoiceHandle::set_loss_keyframe_spacing`).
- A session ended up with every button dead: hover worked, clicks did nothing, while the GTK thread idled normally and the session kept sending video and handling loss reports. The only window-wide click interceptor was the capture-phase "click outside a dialog closes it" gesture, which claimed the click whenever `visible_dialog()` was set and the point fell outside the dialog's child (or its bounds could not be computed). It now never claims, ignores unmapped or empty dialogs, takes only the primary button, and logs when it closes one. Which dialog it believed open was not established (a debugger could not attach: ptrace_scope=1).

## Discord signing an account out (2026-11-18)

- After a failed friend-request captcha, Discord closed the gateway with 4003 then 4004 "Authentication failed", and REST answered 401 for the saved token. The web client showed "Verification Required / Re-verify by Email". So the token is dead (not merely restricted): after re-verifying, the user signs in again with a new token.
- `GatewayEvent::Closed` now carries `fatal`. The account loop used to `break` on every Close frame, fatal or not, so after any ordinary server close the gateway reconnected but nothing read its events any more (the UI said "Reconnecting…" for ever). It now only stops on a fatal close.
- 4004 → `ConnState::SignedOut`: a banner ("Discord signed this account out." + What Now?), the user bar shows the account label and "Signed out", and the "Discord Signed You Out" dialog (`account_notice.rs`) explains it is not a ban, gives the steps, and offers Open Discord (opens discord.com/login, then the token page) / Enter New Token / Not Now. Signing in again drops the dead session of the same user.
- A token that still works but has a `required_action` (READY field, `USER_REQUIRED_ACTION_UPDATE`) gets a "Verification Required" dialog; REST code 40002 is `Error::needs_verification`.
- `adw::AlertDialog` ignores `set_content_width` and stays 372 px; notices that need width use a plain `adw::Dialog` (`notice_dialog`).

## Keyframe safety net (2026-11-18)

- An official viewer (the user's alt, on the same lossy local network) froze and went to endless loading while another viewer saw the stream clean; turning the video off and on at the viewer fixed it at once. The log showed steady 30 fps, loss 0–1% all repaired by RTX, a keyframe every ~4 s (one per NACK at the 1 s spacing), and no PLI from that viewer for 7 minutes. So nothing wrong was sent; the fault lay between Discord and that viewer. Loss bursts hit camera and stream at the same instants, i.e. the local network.
- On-demand keyframes are no guarantee for a viewer whose own download loses packets and whose request never arrives. `VideoSettings.keyframe_gap_secs` (pref of the same name, default 5; Advanced → "Full Frame at Least": Off/3/5/10/15 s) has the capture thread ask for a keyframe through the usual `request_keyframe` path whenever the encoder has produced none for that long (`VideoSource::set_keyframe_gap`, set by the session for camera and stream). Any keyframe resets the timer. Logged as "no keyframe for a while; sending one". Whether these hitch official viewers the way encoder-scheduled ones did is not yet known.
- Idea not done: skip the loss keyframe when every NACKed packet was resent (it is unknown whether RTX alone unfreezes official viewers).

## Call view rebuilt as tiles (2026-11-18)

- `voice.rs`: every person is a tile and every stream is a tile of its own (streams first). `tile_face` decides what a tile shows: `Video`, `Avatar`, `Connecting` (watching, no picture yet), `NotWatching` (Watch button), `OwnStream` (preview off), `PoppedOut`. One function, `call_tile`, draws all three roles: `Grid`, `Mini` (176x99, in the row under an enlarged tile), `Stage` (enlarged, with pop-out / back-to-grid / Stop Watching and the Hide Members toggle; `members_hidden` is not saved).
- `tile_grid.rs`: `TileGrid`, the first GObject subclass in the project (a `gtk::Widget` that lays its children out itself). `arrange` picks the column count giving the largest 16:9 tiles for the room; a short last row is centred; below 160x90 tiles stop shrinking and the call's scroller takes over (height-for-width minimum).
- Pop-out: `pop_out_video(key, title)` shows the same paintable in a `gtk::Window` (double-click / F / F11 fullscreen); closing it, Pop In, or the video ending puts it back. `popouts` holds them; they close with the main window. Also in the user menu (Focus / Pop Out / Pop Back In, for stream and camera separately).
- `KYLECORD_VIDEO_DEMO=grid|focus|hidden|popout|sequence` draws a made-up six-person call with test-pattern video and needs no account: run with `XDG_CONFIG_HOME` and `XDG_CACHE_HOME` pointed at empty directories (the latter also keeps the real log from being rotated) plus `KYLECORD_SCREENSHOT`.
- Reported bug "watch a stream, enlarge a camera, put it back: stream gone and no Watch button" was not reproduced or explained in the old code (the log shows the stream's video never stopped). The new tiles cannot be in that state: a stream tile always shows a picture, Connecting, or Watch. `sequence` covers the steps.
- Per-user volume: top of the slider is `volume_boost_cap` (Features → Maximum User Volume, 1×–5×, default 3×); kc-media clamps at `MAX_USER_VOLUME` 5.0. `adw::AlertDialog` aside, `adw::ComboRow` ellipsizes long values, so keep them short.

## Go Live sound sources (2026-11-18)

- Share popup → Stream Audio: No Sound / Desktop Audio / One Application (+ program list and refresh) / Patchbay Node. Pref `share_audio` (0–3, default 1) plus `share_audio_app` (key) and `share_audio_app_name`; `CapturePreferences::audio_share()` gives `Option<AudioShare>` for `StartStream`/`StartStreamCamera`.
- `kc_media::share_audio`: every mode records a **virtual output** of kylecord's own (`support.null-audio-sink`, media.class Audio/Sink, description "kylecord Go Live") from its monitor with `pulsesrc device=<name>.monitor`. The node is held by a `pw-cli` child (`VirtualOutput`) fed `create-node …` on stdin. It is killed through its handle on drop, and by `PR_SET_PDEATHSIG` if kylecord dies (set in `pre_exec`; the spawning thread lives as long as the child, since the death signal follows the thread). pw-cli does NOT exit when its stdin closes. Checked: killing the probe with -9 removes the node.
- `pipewiresrc autoconnect=false` (as a stream or as Audio/Sink) does not reach PLAYING until something is linked to it: basesrc holds the live lock while pipewiresrc waits for format negotiation. Don't use it for an unlinked node.
- Desktop and One Application: `Linker` runs `pw-dump` every 1.5 s and `pw-link <out port id> <in port id>` for missing links (FL/FR by `audio.channel`; MONO/FC to both; LFE dropped). Desktop = Stream/Output/Audio nodes linked into the default output (`default.audio.sink` metadata, which keeps its props at the top level, not under `info`), minus kylecord (own pid, or application.name "kylecord"). One Application = streams whose `application.process.binary` (else application.name) equals the key. The list shows only streams with a process id, which drops patchbay keep-alive `pw-cat` streams. Without the tools, Desktop falls back to `@DEFAULT_MONITOR@` (kylecord included).
- Patchbay Node uses the fixed name `kylecord-go-live` so a patchbay can remember connections; linker modes use `kylecord-go-live-<pid>-<n>`.
- `kc-probe share-audio list | desktop | node | app <key> [seconds]` captures without sending and prints frames and RMS each second. For a test source that plays nowhere use `pw-cat --playback --target 0 --properties '{ application.name = "x" }' file.wav`; a `pipewiresink mode=provide` fake app stalls once linked.
- The user's machine runs Patch Space (their patchbay) as the default output; see the warnings in memory about never pattern-killing `pw-cli` and never mixing a nix-shell interpreter with the dev shell's GStreamer plugin path.
- flake: `gappsWrapperArgs = [...]` as a derivation attribute is ignored by wrapGAppsHook (the GStreamer path in the launcher came from the hook's own handling). Extra launcher arguments go in `preFixup` with `gappsWrapperArgs+=(...)`; checked in `result/bin/kylecord`.

## Stream brightness (2026-11-19)

- User suspected the camera's limited-range fix also brightened Go Live screens. It does not: only `pipeline_downstream` (cameras, and the Camera option of Go Live) forces `colorimetry=bt709/bt601`; `screen_description` (portal screens) does not. Measured through the screen pipeline with a BGRx source: black→Y16, 64→Y71, white→Y235; decoded back 0 / 62 / 253, whether the decoder honours the declared range or assumes limited.
- Encoder-input caps carry no colorimetry field; GStreamer falls back to the default for the format and size (I420 ≥720p → bt709 limited). Each capture now logs "capture formats" on its first frame: `captured` (what entered videoconvert) and `encoded`, with the effective colorimetry (`describe_video_caps`). For a screen expect `BGRx … sRGB` → `I420 … bt709`. If the compositor sends YUV with a wrong range label, it shows up here.
- Not yet known: where the user saw the brightening (official viewer, kylecord's own view, or their friend's).

## Go Live stuck after closing the screen picker (2026-11-19)

- Log: picker cancelled → the StreamSource error branch called `teardown_stream` (local only, no `stream_delete`) → the next Go Live's `stream_create` got no STREAM_CREATE back (the old stream still existed on Discord) → `stream_pending` stayed true and every later Go Live returned silently until restart.
- Fixed: a failed or cancelled screen source and `StreamClosed` both use `stop_stream` (sends `stream_delete`). A portal cancel is `kc_media::Error::Cancelled` (ashpd `ResponseError::Cancelled`) and shows no error. Every Go Live arms `StreamStartTimeout { attempt }` (15 s); if Discord still has not answered, the attempt is given up with an error. The control handling moved out of the account loop into `handle_voice_control` so tests can drive it.

## Hitch theories, delay default, send-twice, loss-position logging (2026-11-19)

- A 24-minute 720p30 Go Live at a fixed bitrate (~3.9 Mbit/s, set one-time in the popup; REMB 4200) lost a steady 3.3% in single packets, all resent. 46% of the lost packets were index 0 of their frame (only 557 of 28,291 were in keyframes). Cause unknown. With the 1 s loss-keyframe spacing that meant 1,365 keyframes (about one a second). An earlier call at ~1.9 Mbit/s lost 0.10%, in bursts.
- The Go Live delay (`DelayedSender`) also delays forced keyframes, so any freeze that needs a keyframe lasts at least the delay. Default is now 0; the pref is saved as `go_live_delay_ms` so the old 1 s default does not carry over (`stream_delay_ms` in old files is ignored).
- "Send Packets Twice" (Advanced, pref `send_twice`, off by default): `Redundancy` queues each video packet's second copy 20 ms later as an RTX packet through the pacer (pacing rate doubles). `VoiceHandle::set_redundancy`. Whether Discord's SFU uses unsolicited RTX is NOT yet confirmed; kylecord's own receiver ignores duplicates. Log: `sent_twice` in "sent video".
- To explain the first-packet losses: each NACK entry is now `seq:index/count[K]@age,p<pause ms before it was sent>,b<bytes>`, and "sent video" counts `nack_first` and `nack_after_pause` (pause ≥ 5 ms). If lost packets mostly follow a pause, look at what a sender-side pause does (pacer credit lets the first ~3 packets of a frame leave back to back; NIC/Wi-Fi wake-up); if not, at their size.
- Done right after: a loss keyframe is sent only when a resend cannot repair the loss (`loss_keyframe`): a NACKed packet is no longer in the cache (at once), or the same packet is NACKed again within 1 s (spaced by `loss_keyframe_spacing`, owed if too soon). Logged as "a resend cannot repair this loss; sending a keyframe". Still unconfirmed whether a resend alone unfreezes official viewers: if it does not, expect `keyframe_requests` (viewer PLIs) to rise in "sent video" compared with the 5 in 24 minutes seen before, and reconsider.
- The ping graph measures the voice gateway (TCP websocket) every 5 s; it cannot show short UDP loss, which is what causes hitches.
- The 5 s keyframe safety net showed as a pulse of compression artifacts every 5 s (user report, 2026-11-19). "Full Frame at Least" now defaults to Off; the pref is saved as `full_frame_gap_secs` so the old 5 does not carry over. With that, a clean stream sends keyframes only on a viewer's request or a loss a resend cannot repair.
