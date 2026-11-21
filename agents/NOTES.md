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

Captured with `KUBECORD_DUMP_DISPATCH=<dir>` (writes `<EVENT>.json` per
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

- `KUBECORD_SCREENSHOT=<png>` renders the window to a PNG after 7 s and quits.
- `KUBECORD_OPEN=<channel id>` opens that channel once the account is ready.
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
  exits. `KUBECORD_SCREENSHOT` runs are `NON_UNIQUE` so they work while the
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
  receive path; point `KUBECORD_PLAYBACK_DEVICE` / `KUBECORD_MONITOR_DEVICE`
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
  (offline, loses every Nth packet), `KUBECORD_VIDEO_DEMO=1` (test pattern
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
- Dev aid: `KUBECORD_SETTINGS=account|voice` opens the panel 4 s after launch (pair with `KUBECORD_SCREENSHOT`).

## Camera and Go Live popouts (2026-11-18)

- The camera and screen buttons always open a popout (`kc-ui/src/video_setup.rs`) before anything is sent: "Ready to video chat?" and "Share Your Screen". Both offer an optional preview, the source, and one-time quality and Advanced settings (a copy of the prefs; Settings holds the permanent values). The share popout adds source type (screen/window/camera), Go Live delay, and a system-audio switch (`StartStream { audio }`).
- A previewed screen goes live without a second picker by handing over the running capture, not by reconnecting. The preview's `VideoSource` sends frames through a `FrameTap`. Go Live passes `PreparedCapture { source, tap }` in `Command::StartStream { capture }`, and the session points the tap at its sender and forces a keyframe. Reconnecting to the portal node does not work on every portal: the node vanished after the preview disconnected, and pipewiresrc fell back to the default camera. Screen captures now set `node.dont-fallback`/`node.dont-reconnect` in `stream-properties`. A quality change stops a screen preview, which can't be reopened without the picker.
- `quality_controls(initial, stream, apply)` builds the quality rows for both Settings (`apply` = `update_prefs`) and the popouts (`apply` = their local copy).
- Clicking outside the visible `adw::Dialog` closes it (capture-phase `GestureClick` on the window in `connect_signals`).
- A wrapping label in a horizontal box needs `max_width_chars(1)` + hexpand, otherwise its height is measured at full width and GTK logs "Allocation height too small".
- Dev aid: `KUBECORD_POPUP=camera|share|profile` opens that popout 4 s after launch.

## Profile popout (2026-11-18)

- `kc-ui/src/profile.rs` replaces the old card in members.rs; `show_profile(user_id, anchor)` keeps its signature. Left: a banner (image, else the theme/accent colour, else a colour picked from the user id) with the avatar over it; name, original names, the actions, bio (rendered with `rich`), member/friends since, roles, note. Right: Activity / Mutual Friends / Mutual Servers in an `adw::InlineViewSwitcher`.
- Server/Global (`adw::ToggleGroup`) shows only when they are a member of the current server: cached, or the profile response has `guild_member`. Server look = `guild_member_profile` / `guild_member`, each field falling back to the global one.
- The name on top is server nick > friend nick > global name; the username line is always shown, plus the global name when it differs.
- Nicknames (pencil button): friend nickname via `PATCH /users/@me/relationships/{id} {nickname}` (`set_friend_nickname`); server nickname needs CHANGE_NICKNAME (self) or MANAGE_NICKNAMES plus a higher top role (others). Owner and administrator can always.
- Widgets hold the `Profile` weakly; the dialog's `closed` handler holds the only strong ref, so a closed profile is freed. Rebuilds after a signal go through `idle_add_local_once`, because the card owns the widget emitting it.
- Dev aids: `KUBECORD_POPUP=profile|profile-guild|profile-friend` (own; own inside the first server; first friend inside the first server).

## Captchas (2026-11-18)

- Friend requests can get a captcha challenge: a 400 whose body is `{"captcha_key": [...], "captcha_sitekey", "captcha_service": "hcaptcha", "captcha_rqdata", "captcha_rqtoken"}`, with no `message`/`code`. Confirmed with `kc-probe befriend <username>` (PUT relationships). kc-http turns it into `Error::Captcha`; other non-standard error bodies now show up in the message instead of being dropped.
- With the `captcha` feature (kc-ui; on by default in the kubecord binary and the Nix package, which adds webkitgtk_6_0 + glib-networking), `AppState::with_captcha(prompt, request, done)` runs a request. On `Error::Captcha` it shows the hCaptcha in a WebKit view and retries with `X-Captcha-Key`/`X-Captcha-Rqtoken` (`kc_http::CaptchaAnswer`). The page is loaded via `load_html(.., base_uri = "https://discord.com/")`, so hCaptcha's site check passes: Discord's real key `a9b5fb07-…` renders a normal checkbox there. The view uses an ephemeral network session and the client's own user agent. Confirmed working end to end on a live friend-request challenge (2026-11-18).
- Off (Settings → Compatibility), closed unsolved, or built without the feature: the caller gets the challenge error and shows `captcha_needed` (a toast with Open in Browser). The test button and `KUBECORD_POPUP=captcha` use hCaptcha's public test key. `KUBECORD_SCREENSHOT_DELAY` waits longer for network pages.
- kc-probe takes the token from `KUBECORD_TOKEN`. Set it from `~/.config/kubecord/accounts.json` with jq, without printing it.

## DM calls in the chat column (2026-11-18)

- The chat column is a vertical `gtk::Paned`: `call_area` (the voice tiles) above `chat_box` (messages + composer). `voice::call_layout` decides: voice-only channel → `Only`; a DM whose call has anyone in it (you or not) → `WithChat`; otherwise `Hidden`. `render_messages`, `refresh_voice_view` and `render_voice_view` all go through it, so hanging up or the last person leaving takes the call away. The height is saved as `call_height`.
- Discord only sends DM-call voice state updates to the people in the call. The store seeds participants from `CALL_CREATE.voice_states` and clears the channel's voice states on `CALL_DELETE`, which arrives when the call ends.
- A DM call you are not in shows a Join Call button under the tiles. Dev aids: `KUBECORD_POPUP=call-dm` / `call-dm-end` (a local fake CALL_CREATE, then CALL_DELETE).

## Webcam framerate and self-preview (2026-11-18)

- Webcams must be captured as MJPEG: a C920 gives raw 720p at 10 fps (USB 2 bandwidth) but MJPEG 720p at 30. The old `video/x-raw` capsfilter got the 10 fps mode and `videorate` padded it with duplicates sent three at a time, which viewers saw as stutter. `camera_inputs` lists what to ask for, best first (MJPEG exact, larger MJPEG, MJPEG any rate, raw exact, raw ≥640 wide, any raw); every entry but the last must produce a frame (`wait_for_first_frame`, up to 15 s). A format the node lacks is refused at once ("no more input formats"), so silence means a slow camera, not a wrong format; one silent format skips to the last resort. `VideoSettings.camera_format` (pref `camera_format`: 0 auto, 1 MJPEG, 2 raw) narrows the list.
- `kc-probe camera-timing [seconds] [name part] [cpu|auto] [kbit/s]` runs a camera through the real pipeline and prints frame gaps, fps and kbit per second. `RUST_LOG=kc_media=info` shows the format chosen. The C310 (046d:081b) takes about 8-10 s to deliver its first frame. In the probe its frames then arrive in bursts (about 85 real frames every ~3 s, nothing between) in both MJPEG and raw, while the C920 is even; where they are held is not found yet. Camera pipelines use `videorate skip-to-first=true`, or the warm-up time is filled with a burst of duplicates.
- Self-preview: `LocalPreview` (session.rs) sits in the camera and stream frame callbacks, pushes the encoded frames into a `VideoFeed` and reports them as `VideoStarted`/`VideoStopped` for your own user id, so the tiles show them like anyone's video. It starts on a keyframe and ends when the capture is dropped. `set_self_preview` (pref `self_preview`, the tile menu checkbox, Settings → Voice & Video) gates it live. Not yet seen in a real call.

## Camera frame damage, colour range, call log (2026-11-18)

- A webcam's MJPEG frame that loses a USB packet still arrives, shorter; `jpegdec` decodes it with garbage and gray from the hole on (a libjpeg warning, not an error). `kc_media::mjpeg::check` walks the entropy-coded data (Huffman codes, 64-coefficient blocks, end marker right after the last block) and a pad probe on `jpegdec name=mjpeg` (`guard_mjpeg`) drops damaged frames; `videorate` repeats the previous one. Frames it cannot walk (progressive, no DHT of their own) pass unchecked; if most of the last 60 frames fail, the check switches itself off.
- PipeWire hands camera frames over in full-size buffers (1,843,200 bytes for 720p) with stale data after the JPEG, so "ends with FFD9" is not a usable truncation test.
- MJPEG decodes to full-range YUV. The encoder input caps now carry `colorimetry=bt709` (bt601 below 720p), so `videoconvert` converts to limited range; before, Discord's decoder stretched the full-range picture and viewers saw it darker with crushed shadows.
- `~/.cache/kubecord/kubecord.log` (previous run in `.log.1`, mode 0600) records info-level logs of every crate. kc-voice logs a "sent video" line every 5 s while sending: frames, kbit, dropped, nack_requested/nack_found, keyframe_requests, backlog_max_ms, loss_max_percent, remb_kbit. Far-end camera freezes that do not show in the self-preview are still undiagnosed; read this log after a call that had them.

## Ping and the first call log (2026-11-18)

- Ping: the time from a voice gateway heartbeat (op 3, sent every 5 s, more often than the server's interval) to its acknowledgement (op 6). The server does not echo an eight-byte counter keepalive over UDP (tried: no replies), so the UDP keepalive is back to Abaddon's two bytes every 10 s. `VoiceEvent::Ping { millis }` (None when a heartbeat went unacknowledged) → `UiEvent::VoicePing` → `kc-ui/src/ping.rs`: signal bars in the voice panel and a five-minute graph in its popover. Dev aid: `KUBECORD_POPUP=ping` (made-up samples).
- First logged camera call (130 s, 1 Mbit/s, C920 MJPEG): 150 frames every 5 s, no drops, backlog under 30 ms, no keyframe requests after the first, REMB 9 Mbit/s. Six bursts of video NACKs (1, 2, 6, 19 with 4% reported loss, 8, 4), all answered from the retransmit cache. So the far-end hitches line up with short packet-loss bursts on the uplink, repaired by retransmission; whether the server had to ask twice is now counted as `nack_repeated`.
- x264 with `tune=zerolatency` slices each frame per thread: about 13 RTP packets for a 4 KB frame. Not changed.

## Video retransmission must be RTX (2026-11-18)

- Hitch times the user noted matched "video packets asked for again" log lines to the second: 1-4 packets lost on the uplink, all found in the cache and resent, never asked for twice, and yet the viewer froze about 2 s (the keyframe interval) where the official client recovers in 0.25-0.5 s. The resend was a copy of the original packet on the main SSRC. Discord repairs video as RFC 4588 RTX: on the stream's `rtx_ssrc`, payload type 102, its own sequence numbers, the original sequence number in the first two payload bytes (the same form the server sends us). `SessionState::retransmission` now builds that from the cached pre-SRTP payload (`SentVideo`); audio is still resent as it was. A NACK for packets no longer cached forces a keyframe.
- `KUBECORD_TEST_DROP_VIDEO=n` skips sending every nth video packet, to test repair without waiting for real loss. Not yet confirmed against a live viewer.
- Forced-loss run (`KUBECORD_TEST_DROP_VIDEO=300`) after the RTX change: the server asked for each withheld packet once, every one was resent as RTX and almost never asked for twice, no viewer keyframe requests arrived, and the viewer still froze 0.5-2 s per loss (until the next scheduled keyframe). So neither a plain resend nor RTX has been seen to unfreeze an official-client viewer, and no PLI reaches us during those freezes. Every video NACK now also forces a keyframe (`VoiceEvent::ForceKeyframe`); `VideoSource::force_keyframe` spaces them 750 ms apart and defers, rather than drops, a request inside that time. Non-REMB picture feedback (RTCP 206) is logged at info with its media SSRC, to learn whether viewers' requests arrive under an SSRC we ignore.
- Forcing a keyframe on every video NACK backfired: from the first loss on, the server asked for about 29 single packets a second (RR loss 7-9%, roughly one keyframe's worth of packets per forced keyframe, 750 ms apart) where calls before had one small burst every ~10 s. Why forced keyframes are followed by resend requests is not understood. Loss-triggered keyframes are now at least 3 s apart (`LOSS_KEYFRAME_SPACING`). The NACK log line lists each packet as `sequence:index/count[K]@age` (place in its frame, K for a keyframe, time since first sent), and each keyframe sent is logged, to find out which packets the server asks for.

## Camera hitches follow scheduled keyframes (2026-11-18)

- With every keyframe logged, the hitch times the user noted did not match resend requests (3 in 75 s, at other times) but each fell 0.4-1.4 s after a scheduled keyframe, in two separate calls, and the hitches lasted about the keyframe interval (2 s). Reading: about one scheduled keyframe in six is not accepted by an official-client viewer, which then shows nothing new until the next one. Why is not known. The viewer sends no NACK or PLI in that time (libwebrtc waits 3 s before asking).
- Ruled out: the end-to-end encryption step. 3,003 recorded camera frames (`KUBECORD_DUMP_FRAMES=<file>` with `kc-probe camera-timing`) replayed through davey's Encryptor → WebRTC-style packetise/reassemble → Decryptor all decrypt, in order and with video delayed behind interleaved audio (the nonce counter is shared). `rtp::packetize` reproduces encrypted frames byte for byte (brute-force test in rtp.rs). Camera frames here are an AUD plus 11 slices, long start codes throughout, slices of 6-9 bytes in repeated frames.
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
- `KUBECORD_VIDEO_DEMO=grid|focus|hidden|popout|sequence` draws a made-up six-person call with test-pattern video and needs no account: run with `XDG_CONFIG_HOME` and `XDG_CACHE_HOME` pointed at empty directories (the latter also keeps the real log from being rotated) plus `KUBECORD_SCREENSHOT`.
- Reported bug "watch a stream, enlarge a camera, put it back: stream gone and no Watch button" was not reproduced or explained in the old code (the log shows the stream's video never stopped). The new tiles cannot be in that state: a stream tile always shows a picture, Connecting, or Watch. `sequence` covers the steps.
- Per-user volume: top of the slider is `volume_boost_cap` (Features → Maximum User Volume, 1×–5×, default 3×); kc-media clamps at `MAX_USER_VOLUME` 5.0. `adw::AlertDialog` aside, `adw::ComboRow` ellipsizes long values, so keep them short.

## Go Live sound sources (2026-11-18)

- Share popup → Stream Audio: No Sound / Desktop Audio / One Application (+ program list and refresh) / Patchbay Node. Pref `share_audio` (0–3, default 1) plus `share_audio_app` (key) and `share_audio_app_name`; `CapturePreferences::audio_share()` gives `Option<AudioShare>` for `StartStream`/`StartStreamCamera`.
- `kc_media::share_audio`: every mode records a **virtual output** of kubecord's own (`support.null-audio-sink`, media.class Audio/Sink, description "kubecord Go Live") from its monitor with `pulsesrc device=<name>.monitor`. The node is held by a `pw-cli` child (`VirtualOutput`) fed `create-node …` on stdin. It is killed through its handle on drop, and by `PR_SET_PDEATHSIG` if kubecord dies (set in `pre_exec`; the spawning thread lives as long as the child, since the death signal follows the thread). pw-cli does NOT exit when its stdin closes. Checked: killing the probe with -9 removes the node.
- `pipewiresrc autoconnect=false` (as a stream or as Audio/Sink) does not reach PLAYING until something is linked to it: basesrc holds the live lock while pipewiresrc waits for format negotiation. Don't use it for an unlinked node.
- Desktop and One Application: `Linker` runs `pw-dump` every 1.5 s and `pw-link <out port id> <in port id>` for missing links (FL/FR by `audio.channel`; MONO/FC to both; LFE dropped). Desktop = Stream/Output/Audio nodes linked into the default output (`default.audio.sink` metadata, which keeps its props at the top level, not under `info`), minus kubecord (own pid, or application.name "kubecord"). One Application = streams whose `application.process.binary` (else application.name) equals the key. The list shows only streams with a process id, which drops patchbay keep-alive `pw-cat` streams. Without the tools, Desktop falls back to `@DEFAULT_MONITOR@` (kubecord included).
- Patchbay Node uses the fixed name `kubecord-go-live` so a patchbay can remember connections; linker modes use `kubecord-go-live-<pid>-<n>`.
- `kc-probe share-audio list | desktop | node | app <key> [seconds]` captures without sending and prints frames and RMS each second. For a test source that plays nowhere use `pw-cat --playback --target 0 --properties '{ application.name = "x" }' file.wav`; a `pipewiresink mode=provide` fake app stalls once linked.
- The user's machine runs Patch Space (their patchbay) as the default output; see the warnings in memory about never pattern-killing `pw-cli` and never mixing a nix-shell interpreter with the dev shell's GStreamer plugin path.
- flake: `gappsWrapperArgs = [...]` as a derivation attribute is ignored by wrapGAppsHook (the GStreamer path in the launcher came from the hook's own handling). Extra launcher arguments go in `preFixup` with `gappsWrapperArgs+=(...)`; checked in `result/bin/kubecord`.

## Stream brightness (2026-11-19)

- User suspected the camera's limited-range fix also brightened Go Live screens. It does not: only `pipeline_downstream` (cameras, and the Camera option of Go Live) forces `colorimetry=bt709/bt601`; `screen_description` (portal screens) does not. Measured through the screen pipeline with a BGRx source: black→Y16, 64→Y71, white→Y235; decoded back 0 / 62 / 253, whether the decoder honours the declared range or assumes limited.
- Encoder-input caps carry no colorimetry field; GStreamer falls back to the default for the format and size (I420 ≥720p → bt709 limited). Each capture now logs "capture formats" on its first frame: `captured` (what entered videoconvert) and `encoded`, with the effective colorimetry (`describe_video_caps`). For a screen expect `BGRx … sRGB` → `I420 … bt709`. If the compositor sends YUV with a wrong range label, it shows up here.
- Not yet known: where the user saw the brightening (official viewer, kubecord's own view, or their friend's).

## Go Live stuck after closing the screen picker (2026-11-19)

- Log: picker cancelled → the StreamSource error branch called `teardown_stream` (local only, no `stream_delete`) → the next Go Live's `stream_create` got no STREAM_CREATE back (the old stream still existed on Discord) → `stream_pending` stayed true and every later Go Live returned silently until restart.
- Fixed: a failed or cancelled screen source and `StreamClosed` both use `stop_stream` (sends `stream_delete`). A portal cancel is `kc_media::Error::Cancelled` (ashpd `ResponseError::Cancelled`) and shows no error. Every Go Live arms `StreamStartTimeout { attempt }` (15 s); if Discord still has not answered, the attempt is given up with an error. The control handling moved out of the account loop into `handle_voice_control` so tests can drive it.

## Hitch theories, delay default, send-twice, loss-position logging (2026-11-19)

- A 24-minute 720p30 Go Live at a fixed bitrate (~3.9 Mbit/s, set one-time in the popup; REMB 4200) lost a steady 3.3% in single packets, all resent. 46% of the lost packets were index 0 of their frame (only 557 of 28,291 were in keyframes). Cause unknown. With the 1 s loss-keyframe spacing that meant 1,365 keyframes (about one a second). An earlier call at ~1.9 Mbit/s lost 0.10%, in bursts.
- The Go Live delay (`DelayedSender`) also delays forced keyframes, so any freeze that needs a keyframe lasts at least the delay. Default is now 0; the pref is saved as `go_live_delay_ms` so the old 1 s default does not carry over (`stream_delay_ms` in old files is ignored).
- "Send Packets Twice" (Advanced, pref `send_twice`, off by default): `Redundancy` queues each video packet's second copy 20 ms later as an RTX packet through the pacer (pacing rate doubles). `VoiceHandle::set_redundancy`. Whether Discord's SFU uses unsolicited RTX is NOT yet confirmed; kubecord's own receiver ignores duplicates. Log: `sent_twice` in "sent video".
- To explain the first-packet losses: each NACK entry is now `seq:index/count[K]@age,p<pause ms before it was sent>,b<bytes>`, and "sent video" counts `nack_first` and `nack_after_pause` (pause ≥ 5 ms). If lost packets mostly follow a pause, look at what a sender-side pause does (pacer credit lets the first ~3 packets of a frame leave back to back; NIC/Wi-Fi wake-up); if not, at their size.
- Done right after: a loss keyframe is sent only when a resend cannot repair the loss (`loss_keyframe`): a NACKed packet is no longer in the cache (at once), or the same packet is NACKed again within 1 s (spaced by `loss_keyframe_spacing`, owed if too soon). Logged as "a resend cannot repair this loss; sending a keyframe". Still unconfirmed whether a resend alone unfreezes official viewers: if it does not, expect `keyframe_requests` (viewer PLIs) to rise in "sent video" compared with the 5 in 24 minutes seen before, and reconsider.
- The ping graph measures the voice gateway (TCP websocket) every 5 s; it cannot show short UDP loss, which is what causes hitches.
- The 5 s keyframe safety net showed as a pulse of compression artifacts every 5 s (user report, 2026-11-19). "Full Frame at Least" now defaults to Off; the pref is saved as `full_frame_gap_secs` so the old 5 does not carry over. With that, a clean stream sends keyframes only on a viewer's request or a loss a resend cannot repair.

## Automatic ranges for bitrate and framerate (2026-11-19)

- Bitrate on Automatic has a range: prefs `auto_bitrate_min` / `auto_bitrate_max` (`None` = left to kubecord: top from resolution and framerate, bottom three tenths of the top). `VideoSettings.bitrate_floor_kbps` (0 = default) and `bitrate_follows_estimate`: a top set by hand is NOT capped at 95% of REMB (Discord's REMB for a Go Live sat at 4200 kbit/s while the user found 6000 at 24 fps looked good); the automatic top still is.
- Framerate can be Automatic (prefs `fps_auto`, `fps_auto_min`, `fps_auto_max`; `VideoSettings.fps` is then the top and `fps_floor` the bottom). It follows the same controller: `BitrateController::level()` (1 at the top of the bitrate range, 0 at the floor) → `framerate_for(level, min, max)`, the highest of 5/10/15/20/24/30/45/60 (or an end of the range) not above that share. With a fixed bitrate the controller still runs, only to steer the framerate.
- A live framerate change sets new caps on the `name=rate` capsfilter (both pipelines); the encoder restarts its stream with a keyframe, so changes are at least 10 s apart (`FRAMERATE_CHANGE_SPACING`). `VideoSource.frame_ms` follows, so frame durations stay right. Tested on x264enc and vah264enc. `VideoSource::adapt(&settings)` replaces `adapt_bitrate`.
- UI: Framerate and Bitrate each list Automatic; the Lowest/Highest rows show only on Automatic. Not yet seen on screen (below the fold of the dev screenshot) or in a real call.

## Stream information panel (2026-11-19)

- An (i) button in a video tile's controls (hover on grid tiles, always on the enlarged one, hover in a pop-out window) toggles a panel over the picture: size and shown fps, bitrate, frames in/shown, full frames and how long since the last. For your own camera or stream it adds upload loss and where adaptation stands. Not on the small tiles under an enlarged one.
- `video_info.rs`: `VideoInfo` (which keys show, weak refs to the labels, last sample per key, latest `VideoUpload` per own camera/stream) and a pure `describe`. One 1 s timer (`refresh_video_info`, started in app.rs) measures every shown video; tiles are redrawn often, so labels register themselves weakly and pick up the last text.
- Sources: `VideoFeed::stats()` (frames, bytes, keyframes, frames since the last keyframe), decoder's decoded count, the paintable's intrinsic size; `UiEvent::VideoUpload` forwarded from `VoiceControl::VideoNetwork` with `VideoSource::adapted()`.
- `KUBECORD_VIDEO_DEMO=info | info-grid` shows it offline. The pop-out window's button was not captured.
- "When the Connection Struggles" (pref `adapt_priority`: 0 Balanced, 1 Prefer Bitrate, 2 Prefer Framerate; `VideoSettings.adapt_priority`), shown only with both bitrate and framerate on Automatic. `split_level(level, priority)`: Balanced moves both with the controller's level; a priority hands the first half of the way down (level 1 → 0.5) to the other one alone, then lowers its own. The encoder bitrate is now `floor + bitrate_level × (top − floor)` (equal to the controller's own value when Balanced), tracked as `applied_kbps`.
- The range settings are one row each (`range_row`, an `adw::PreferencesRow` with Lowest and Highest side by side); `QualityControls.rows` is now `Vec<gtk::Widget>`.
- Dev aids for long pages: `KUBECORD_WINDOW_HEIGHT=<px>` (window and settings dialog; the window cannot exceed the screen) and `KUBECORD_SETTINGS_SCROLL=<px>`.

## Capture died when the framerate adapted (2026-11-19)

- User log: "video framerate adapted from_fps=30 to_fps=24" was followed within a second by `capture pipeline error … error set output format: -22 (Invalid argument) gstpipewiresrc.c on_state_changed` and "Internal data stream error"; the compositor's capture indicator vanished and viewers got endless loading. Cause: new caps on the `rate` capsfilter send a RECONFIGURE event upstream, and pipewiresrc renegotiates by tearing its stream down. Reproduced without a screen: `cargo test -p kc-media pipewire_capture -- --ignored` (a `videotestsrc ! pipewiresink mode=provide` child as the source) gave 59 frames in 2 s before the change and 13 after.
- Fix: `hold_source_format` puts a probe on the sink pad of the videorate (`name=pace`, both pipelines) that drops upstream RECONFIGURE events, installed at the first framerate change. videorate takes any input rate, so nothing upstream needs to change. Same test: 59 → 48 (24 fps). The earlier framerate tests used videotestsrc, which renegotiates happily; test live changes against a real pipewiresrc.
- A dead stream capture is now noticed: `VideoSource::failed()` (bus error) or `quiet_for()` ≥ 10 s (a healthy screencast resends a still screen every second) ends the stream with a message. kc-voice sends `VideoNetwork` every 2 s while video is on even when nothing went out, so the check keeps running.
- That log also shows the trigger: bursts where more packets were asked for again than were sent in the window (sent=1271 lost=1654) at a Highest Bitrate of 8000.

## Targets inside the automatic ranges; bitrates to 20000 (2026-11-19)

- Each range row is Target | Lowest | Highest (prefs `fps_auto_target`, `auto_bitrate_target`; `None` = Automatic). Target is used while clean and is where it starts; loss takes it toward Lowest and it returns to Target after 5 clean reports per +10% step; above Target it climbs toward Highest only with 15 clean reports (30 s) per step (`BitrateController::with_range(floor, target, top)`, `level()` below target, `headroom()` above). Above Target, bitrate and framerate climb together.
- Settings mapping: Automatic Target = Highest; Automatic Highest = Target if set; neither = computed from the picture (and kept under Discord's REMB). Either set by hand turns off the REMB cap. So with neither target set, behaviour is unchanged.
- `VideoSettings.fps` is the highest the capture is built for; `fps_target` is applied at `adapt()` via `set_framerate` (through the RECONFIGURE-dropping probe). Checked on a real PipeWire source: 47 frames per 2 s at 24 out of a 30 build, then 40 at the next step down. `bitrate_top_kbps` is the bitrate's highest.
- `BITRATES` now go to 20000 (12000, 15000, 20000 added).
- `KUBECORD_SETTINGS_SCROLL` also scrolls the share and camera popups (`settings_panel::dev_scroll`).

## Signal bars in the status colours (2026-11-19)

- The voice panel's ping bars are 13×11 in a `kc-ping-button` (rounded rectangle, 6 px radius, no `circular`). Lit bars take the icon's CSS colour, set per state by `kc-ping-good` / `kc-ping-fair` / `kc-ping-poor` = `@success_bg_color` / `@warning_bg_color` / `@error_bg_color`, the same as the online / idle / dnd status dots. Unlit bars are faint in the button's colour. "Voice Connected" uses `kc-voice-connected` (`@success_bg_color`) instead of libadwaita's `success`, so a good connection matches it.
- `KUBECORD_VIDEO_DEMO` also shows the voice panel, with a made-up ping from `KUBECORD_DEMO_PING` (ms, default 40).

- Info panel position: on grid tiles and pop-outs the panel sits in the top corner (margin 6) and the `kc-video-info-yields` class adds 34px top margin while the tile/pop-out is hovered, so it moves below the controls. The enlarged tile keeps its controls on the right, so its panel never moves.

- Hardware encoding (vah264enc) never loaded for viewers: WebRTC receivers rewrite any SPS whose VUI lacks a bitstream restriction, allows reordering, or has `max_dec_frame_buffering > max_num_ref_frames` (`SpsVuiRewriter`, run in the H.264 depacketizer). VA-API on AMD declares 2 with 1 reference frame. The SPS is unencrypted but authenticated under DAVE, so the receiver's edit makes every keyframe fail decryption. Official clients rewrite before encrypting; `kc_media::h264::conform` now does the same (same rules), applied to every encoded frame where it leaves the pipeline. x264's SPS already passes and comes back byte for byte.
- `conform` also drops filler NAL units (type 12): AMD CBR pads each frame to its share of the bitrate (914 KB of a 1 MB test clip was filler).
- `vah264enc key-int-max` only goes to 1024; a larger value is refused with a GLib critical and the encoder falls back to a keyframe every second. Clamped (`HARDWARE_KEYFRAME_DISTANCE`), so "only when asked" on hardware still means one every 1024 frames.
- On this GPU a live `bitrate` change makes vah264enc emit a keyframe, and its keyframes are ~3× x264's (167 KB vs 60 KB at 4 Mbit 720p; `cpb-size` has no effect). Adaptive bitrate on hardware therefore changes at most every 10 s (`HARDWARE_BITRATE_SPACING`).
- Default is now the CPU encoder (`cpu_encoder: true`); a saved `false` is kept.
- Not verified against an official client: no second account to watch with. Verified: SPS check on real vah264enc output, independent parse, avdec_h264 decode of the corrected stream.

- The actual reason hardware streams never loaded (found in the user's log, `encryption failed, encrypted frame buffer is too small` on every frame once DAVE was ready): davey, like libdave, leaves 64 bytes (`TRANSFORM_PADDING_BYTES`) after a frame for the nonce plus the LEB128 list of unencrypted ranges, one range per slice. `num-slices=32` (raised from 4 in 21238e3) needs ~130. Frames were dropped before sending; until DAVE became ready they went out in passthrough, which is why the first seconds looked alive. Now `HARDWARE_SLICES = 4`, and `kc_voice::dave::MAX_VIDEO_SLICES = 12` with `video_frame_fits` (a conservative bound) checked in tests for both encoders at 720p and 1080p.
- x264 with `tune=zerolatency` makes one slice per thread, capped by height (11 at 720p, 16 at 1080p on a 16-thread machine). 16 is over the limit, so `threads=` is now capped at 12. CPU 1080p keyframes were probably failing to encrypt on machines with 16+ threads.
- When encryption fails for a frame that does not fit, the connection now warns `video frame is in too many slices to be encrypted` instead of the debug line about waiting for DAVE.
- Lesson: read the log of the failing attempt before theorising. The SPS rewrite (previous note) is real and still needed, but it was not what stopped the stream.
- Ending a stream paused the user's YouTube video: WirePlumber 0.5's `linking/mpris-pause.lua` (`linking.pause-playback`, on by default) pauses MPRIS players whose streams are linked, directly or through other nodes, to an Audio/Sink that is removed. Removing the Go Live null sink with browser streams still linked triggered it. `VirtualOutput::drop` now removes every link into the sink (`pw-link --disconnect`), waits 250 ms, then removes the sink. Checked live with a throwaway silent pw-cat stream: links reach 0 while the node is still present. Not covered: kubecord being killed (PDEATHSIG removes the sink with links in place).

## Mute and deafen shown to others (2026-11-19)

- Mute and deafen only changed the local microphone and the voice server's speaking flag; nobody else saw them. Others see `self_mute` / `self_deaf` from the main-gateway voice state (op 4). `announce_voice_state` (session.rs) sends it for the call being joined or in, on join and on every toggle; deafening also sends `self_mute: true`, as the official client does. Camera updates use the same flags.
- Your own entry in the sidebar and the call tiles uses the local choice (`VoiceUiState::own_flags`) instead of waiting for Discord's echo.
- `GatewayHandle::detached()` gives a handle whose payloads land on a receiver, for tests of what is sent.

## Chat could not reach a message just sent (2026-11-19)

- Reproduced offline with `KUBECORD_DEMO=chat`: after a message was added, the scroll adjustment reported the bottom (`upper - page - value = 0`) while the new row was drawn just below the window (top at the old visible bottom). Cause: the list's `changed` handler set the adjustment value. The viewport emits `changed` at the end of its size allocation (its adjustments are frozen while it allocates), after it has placed the list; the reallocation the new value queues is dropped, so the list stays at the old offset. Scrolling down could not move a value already at the maximum; scrolling up moved it and reallocated. The same applied to keeping the place when older rows are prepended.
- Fix: `queue_scroll_settle` does that work on the next idle (coalesced). Rows that grow late (images, embeds, edits) go through the same path.
- Rule: never set an adjustment's value from its own `changed` (or `value-changed`) handler; defer it, as with visibility changes.
- Dev aid `KUBECORD_DEMO=chat|sidebar|voice-empty|voice-people|voice-joined` (demo.rs): an offline made-up account and server fed through the real store reducers; prints `demo:` scroll measurements on stderr. Run with `XDG_CONFIG_HOME`/`XDG_CACHE_HOME` in a scratch directory plus `KUBECORD_SCREENSHOT`.

## Channel list jumped to the top on a click (2026-11-19)

- Reproduced with `KUBECORD_DEMO=sidebar` (value 199 → 16 after clicking a voice channel near the bottom). The sidebar is rebuilt on every click and store event; the clicked button had taken the focus, and when it was removed GTK handed the focus on and the viewport scrolled to the new focus. Restoring the old value straight after the rebuild was not enough: the scroll to the focus lands later.
- Fix (sidebar.rs `render_channels`): sidebar rows no longer take focus on click; if the focus is inside the list anyway (keyboard), it is cleared before the rebuild; the adjustment value is put back after the rebuild and again on the next idle (the viewport clamps to its old height until it has measured the new rows). Redrawing the same list (`sidebar_shown`: DM mode and guild) keeps its position; another server's list starts at the top. Checked with focus forced onto the row too.
- The demo's READY guild `voice_states` are not read by the reducer (only GUILD_CREATE and READY_SUPPLEMENTAL carry them), so the demo seeds voice states with VOICE_STATE_UPDATE.

## Join Voice on the voice channel page (2026-11-19)

- A voice channel you are not in shows a Join Voice button (pill, suggested-action): under "No One Is Here" ("Nobody is in this voice channel yet.") when it is empty, and under the tiles when others are there. DM calls keep their Join Call button; both come from `voice::join_button`, which disables itself once clicked. Seen with `KUBECORD_DEMO=voice-empty|voice-people`; joining itself was not exercised (no account in the demo).

## 12-hour clock by default (2026-11-19)

- Times of day are `5:58 PM` unless Settings → Features → Time → "Use 24-Hour Time" (pref `use_24_hour_time`, default off, so existing settings files get 12-hour). `content::clock_of` / `clock_text` build every clock: message headers, "Yesterday at", edited tooltips, Discord `<t:…>` timestamps (t, T, f, F, s, S), and the ping graph's minute labels. AM/PM is spelled out, not taken from the locale's `%p` (empty in some locales).
- The time beside a grouped message has only the avatar's 40 px: it drops AM/PM (`gutter_time`) and carries the full time as its tooltip; with the suffix it pushed the message text right.
- Switching the setting redraws the open channel (`redraw_times`); other times update as they are next drawn. Seen with `KUBECORD_DEMO=chat` in both modes (24-hour via a sandbox settings.json); the live switch itself was not watched on screen.

## Calls that ended by themselves; voice reconnects (2026-11-19)

- User report: after a long call with a Go Live, the Voice Connected panel vanished. The log of that run (it reached the 20 MB cap and stops early the next morning) shows a Go Live that ran about two hours, then: the main gateway read failed ("Connection reset by peer"), a resumable reconnect after 1 s with no connect failure and no invalid session logged, and 49 s later the own-stream preview ended and nothing more came from the voice or stream connections. No line says why. Every path that ends a call was silent: a self voice state with no channel, a voice websocket close or error (its close code was dropped), RESUMED, op 7 and op 9. So the cause is NOT established. One candidate the code had: `VOICE_SERVER_UPDATE` during a call was ignored (`maybe_connect` returns when no join is pending), so a voice-server move ended the call as soon as the old server closed the connection.
- Also seen: the gateway's reconnect backoff never reset (1, 2, 4 … 30 s over the day). It now resets after READY or RESUMED.
- Logging added: gateway identify/resume/resumed, op 7, op 9, closes without a frame; the voice server's close code and reason, a lost connection, heartbeat send failures; "call ended" with the reason for every teardown; voice server updates during a call, a new voice session, being moved, the gateway starting a new session during a call; Go Live closes and reconnects.
- Recovery (session.rs): `VoiceHandle` (kc-voice) is now shared: `retarget` moves every clone (microphone, camera, stream video and sound) to a new connection, so a call or a Go Live changes connection without restarting its capture.
  - `VOICE_SERVER_UPDATE` during a call with a different endpoint or token → `reconnect_voice` to the new server (no leave, no UI change). A null endpoint → wait for the next update.
  - Voice close (`voice_close_action`): no close frame, 1000/1001/1006, 4009, 4015 and unknown codes → reconnect to the same server, backing off 1-16 s, 5 tries, then the call is given up and Discord told we left. 4014 and 4022 → wait up to 15 s for the gateway to name a server (or to say we left); 4006 also states the call again (op 4). 4001-4005, 4011, 4012, 4016, 4017, 4020, 4021 → the call ends.
  - READY while in a call (the gateway could not resume): the call is stated again (op 4), the new session's server update reconnects it; a running Go Live is carried (`stream_carry`) and created anew once the call is ready, its capture handed to the new stream connection.
  - Moved to another channel: the connection follows (new DAVE group), a Go Live is ended.
  - The Go Live's own connection: a dropped connection with a running capture is reconnected to its server (same backoff); `STREAM_SERVER_UPDATE` with a new endpoint moves it.
- Covered by tests that drive the account loop's handlers with a detached gateway handle and a voice connection pointed at nothing. Not seen against Discord: whether a resent op 4 for the same channel gets a new server update, and whether re-identifying to the same voice server with the same token works after 4015 or a dropped socket (no voice-gateway RESUME, op 7, is implemented). A late STREAM_DELETE for a re-created stream would end the new one (same key).
- The stream teardown now disconnects before dropping the sound share, which blocks for a moment (the log showed the stream still reporting 1.7 s after its capture was dropped).

## Image viewer no longer blocks the main window (2026-11-19)

- The viewer was an `adw::Window` with `.modal(true)` and `transient_for`; on niri it ended up out of view and the main window ignored every click while it existed. It is now a separate non-modal, non-transient window (`image_viewer.rs`, registered through `windows.rs`: one window per key, presented again instead of duplicated, closed with the main window). Rule: a separate toplevel must never be modal; short prompts are in-window `adw::AlertDialog`s. The only other toplevel, the video pop-out, was already non-modal.
- Zoom: wheel (toward the pointer), pinch, `+`/`-`/`0`, header buttons, double-click fitted ↔ 100 %, drag to pan. Starts fitted (never enlarged); 8× at most. The maths (`zoom_about`, `fit_scale`, `clamp_scroll`) is unit-tested. New scroll offsets are applied after the layout grows the adjustments (from an idle after `changed`), and zooms in quick succession start from those pending offsets, or they drift. `KUBECORD_DEMO=viewer` saves the zoomed viewer to `<screenshot>.viewer.png`.

## Moderation and server administration (2026-11-19)

- Permission logic: `kc_state::permissions` lists every bit plus `GROUPS` (the editor catalogue, `in_channels` marks bits that make sense in overwrites). `kc_state::moderation`: role hierarchy (`member_rank`, `outranks`, `can_manage_role`: owner bypasses, Administrator does NOT lift the hierarchy, nobody acts on the owner, admins cannot be timed out, managed roles and `@everyone` are never assignable) and `State::member_actions(guild, user, now)` which gates every menu item. Voice moderation (move/disconnect/mute/deafen) checks the voice channel's permissions without the hierarchy; Move To lists channels with View + Connect + Move Members.
- REST (kc-http): kick, ban (`delete_message_seconds`), unban, bans, `edit_member(MemberEdit::{Nick, Roles, Timeout, Move, Mute, Deafen})`, per-role PUT/DELETE, roles create/edit/delete/reorder, channel edit/delete, overwrites PUT/DELETE, invites list/create/revoke, audit log. Reasons go in `X-Audit-Log-Reason`, percent-encoded, at most 512 chars. 50035 errors carry the first field error in the message; `Error::describe()` explains 50013 and other common codes. NONE of these writes has been sent to Discord (no test account may be modified); only their payloads are unit-tested.
- UI: the user menu gets the moderation items (`moderation.rs`); Server Settings is an in-window two-pane `adw::Dialog` (`server_settings.rs`) with Overview / Roles (+ role editor) / Members / Invites / Bans / Audit Log, each page shown only with its permission; failures appear as toasts in the dialog and controls snap back. Channel management is on the sidebar's right-click (`channel_settings.rs`). Server mute/deafen show red.
- Reducer: GUILD_UPDATE reads description, AFK channel/timeout, system channel and default notifications; GUILD_BAN_ADD/REMOVE become `StoreEvent::BansUpdated` (bans are not cached; the Bans page reloads). Roles and members were already handled.
- `KUBECORD_DEMO=mod-*|settings-*|invite|channel-*` show all of it offline. The demo's REST client points at `https://127.0.0.1:9`, so nothing in the demo can reach Discord; `mod-error` and `settings-error` show how a failed request looks. Popovers are separate surfaces and are not in the window picture; the demo saves them to `<screenshot>.menu.png`.
- Not done: drag-and-drop of a voice participant was added but never exercised (no pointer in the demo); member search in Server Settings relies on gateway op 8 results arriving as GUILD_MEMBERS_CHUNK.

## Settings pages, voice processing, notifications, sounds (2026-11-19)

- Settings tabs (`settings_panel.rs` `PAGES`): My Account | Voice, Video, Notifications, Sounds, Chat | Advanced (captchas + log). `KUBECORD_SETTINGS=<page>` opens one; old names (`features`, `compat`, `debug`) map through `page_alias`. `KUBECORD_DEMO_LEVEL=<dB>` puts a made-up level on the sensitivity meter.
- Microphone (`kc_media::processing`): process-global `VoiceProcessing` like the devices. The mic `Capture` thread rebuilds its pipeline whenever `processing::generation()` changes (processing chain, input device, or the echo probe appearing/disappearing), so all of it applies live; a processed pipeline that fails to start or errors falls back to the plain one (`Capture::is_processed`). Chain: `webrtcdsp` (noise-suppression / echo-cancel / gain-control) and/or `audiornnoise` (F32, 48 kHz), both already in the flake's gst-plugins-bad / gst-plugins-rs. Echo cancellation needs a `webrtcechoprobe name=kcechoprobe` in a playback pipeline of the same process: call playback (`RemoteAudio`) carries it; webrtcdsp refuses to start without it, so echo-cancel is only put in the chain while a probe exists (`EchoProbeGuard` counts them). Not in the mic test's playback (it would cancel the mic itself). Properties of webrtcdsp are read at setup, hence rebuilds rather than live property changes. Test: `microphone_processing_passes_audio_and_follows_changes` (audiotestsrc through every variant, with a fakesink probe pipeline).
- The microphone is now gated (`VoiceGate`): automatic (noise floor = quickest-falling minimum, rising 1.5 dB/s; speech = 12 dB above it and at least -60 dBFS) or a manual threshold; opens on the first loud frame, holds 400 ms. Closing sends `VoiceHandle::end_talk_burst` (five unencrypted `F8 FF FE` frames, then speaking 0); held-back frames call `skip_audio` so the RTP timestamp keeps time. The gate starts open, so a silent start still ends with silence frames. The local ring follows the gate (the old -38 dB detector is gone). The mic test plays back what would be sent (gated, processed). Not yet heard by another person in a real call.
- Notifications: `kc_state::notify::decide(MessageFacts, NotifyPrefs)` is the whole rule set (own/blocked/@silent/call-start message/viewing/DND → none; DMs unless the DM is muted; level NO_MESSAGES → none; mentions (you, reply pings via `mentions`, your roles unless `suppress_roles`, everyone/here unless `suppress_everyone`) even in muted places; otherwise only ALL_MESSAGES and not muted). `State::notification_level` walks channel override → parent (category; thread → channel → category) → guild setting → `default_message_notifications`; 3 is inherit at every level. `GuildSettings.channel_overrides` values are now `ChannelOverride { mute, message_notifications }`.
- `notifications.rs`: `gio::Notification` through the application, id per channel (`message-<account>-<channel>`, so a burst replaces itself), icon = cached avatar file, click → `app.open-message` with `account:channel:message` (switches account, opens the channel, `jump_or_fetch`). Opening a channel withdraws its notification. Calls: `call-<channel>`, only while the window is not active. GTK 4 has no urgency hint, so "(N) title" in the window title stands in (`set_window_title` keeps the count in front of whatever members.rs sets). Demo runs (`KUBECORD_DEMO`, or `KUBECORD_NOTIFY_DRY`) print `demo: notification …` instead of sending. `KUBECORD_DEMO=notify` walks the cases and clicks one through the real action.
- Channel right-click: Mute/Unmute and the level (Server Default / All / Only @mentions / Nothing) via `PATCH /users/@me/guilds/{guild}/settings` with `channel_overrides: {id: {muted, message_notifications, mute_config}}`; the response is fed to the store as `USER_GUILD_SETTINGS_UPDATE`. Mute Server used `/users/@me/guild-settings/{id}` (not a documented path) and now uses the same endpoint. NONE of these writes has been sent to Discord.
- Sounds (`kc_media::sound`, `kc-ui/src/sounds.rs`): each sound is its own playbin (flags=audio, `pulsesink` with `media.role=event`, the configured output) watched by a thread; loops seek to 0 on EOS. Discord's files are not in the repo or package: `discover()` loads `https://discord.com/app`, reads `/assets/web.*.js` first (then other scripts, at most 40), collects `"./<name>.mp3":"<module>"` and `<module>(e,t,n){…e.exports=n.p+"<hash>.mp3"}` (regexes also take the `id:function(…)` / `id:(…)=>` forms), downloads `https://discord.com/assets/<hash>.mp3` into `~/.cache/kubecord/sounds/discord/`, and saves `manifest.json`. As of 2026-11-19 the main bundle (~13 MB) alone names 89 sounds and all 14 kubecord uses; `cargo test -p kc-ui discord_sounds -- --ignored` checks it live (and decodes one into a fakesink). If Discord changes how the bundle is built, discovery fails and built-in tones (WAVs generated by `tone_wav`) play; Refresh Sounds retries. Overrides: `~/.config/kubecord/sounds/<event key>.{mp3,ogg,wav,flac,opus}`.
- Sound triggers: VoiceJoined only when the UI's voice channel changes (it is reported several times per call); `VoiceLeft { lost }` (`call_lost` set by `give_up_call`) picks the disconnect sound; others joining/leaving your call are diffed from voice states; viewers from `Stream.viewer_ids` (STREAM_UPDATE) of your own stream key; outgoing ring stops when someone joins, when `CallRinging.anyone` turns false (after a 4 s grace: the call is created before we ring), on leave, or after 60 s. `KUBECORD_DEMO=sounds` prints each sound.

## Email & password login (2026-11-19)
- `kc_http::auth`: the logged-out web-client flow. `AuthClient` (no `authorization`; same browser headers and super properties as `HttpClient` via the shared `browser_headers`, referer `https://discord.com/login`, `X-Fingerprint` from `GET /experiments`, fetched once per sign-in and reused for the code step). `POST /auth/login {login, password, undelete:false, login_source:null, gift_code_sku_id:null}`; codes `POST /auth/mfa/{totp,backup,sms}` `{code, ticket, login_source:null, gift_code_sku_id:null}`; `POST /auth/mfa/sms/send {ticket}` → `{phone}`. Captcha retries put `captcha_key`/`captcha_rqtoken` in the body and in the `X-Captcha-*` headers. Auth requests are never retried (a 429 comes back as `Rejection::RateLimited`).
- `parse_response` (pure) → `AuthResponse::{Token, Mfa, Captcha, Rejected(..)}`; `Rejection` covers field errors (`errors.login/password/code._errors`), `ACCOUNT_LOGIN_VERIFICATION_EMAIL`, `ACCOUNT_DISABLED`/`ACCOUNT_SCHEDULED_FOR_DELETION`, 429, 60006 / `errors.ticket` (ticket expired), 60008 (wrong code). Messages come from Discord's `message`/field text, never the raw body. The client turns `Captcha` into `Error::Captcha`, so `AppState::with_captcha` handles it like friend requests.
- Secrets: `Password` zeroizes on drop and prints redacted; `MfaChallenge` hides its ticket; request bodies and the response bytes are `Zeroizing`. reqwest's own body copy and GTK's `GString` from the entry are not wiped. Nothing logs a body; the log file only takes other crates' warnings anyway.
- `kc-ui/src/login.rs`: the page (`LoginPage`, replacing the old token widgets on `AppState`) with an `adw::ToggleGroup` (Email & Password | Token), `adw::PasswordEntryRow`, per-field error labels, spinner while busy, code step with Back and "Text Me a Code". One code field takes 6 digits (TOTP, or SMS once a text was sent) or an 8-character backup code (`code_method`). `advance(step, result) → Next::{SignedIn, Show(step, feedback)}` is the tested state machine. A token from either form goes through `Command::Login` (checks `/users/@me`, saves exactly as before). The password field is emptied as soon as it is read. The signed-out dialog opens the Token form.
- `KUBECORD_DEMO=login-<password|token|add|busy|errors|location|code|code-sms|code-wrong|expired|submit>` shows each state offline; in demo runs `AuthClient` points at `https://127.0.0.1:9`.
- Never tried against Discord (it may flag the account): the live request shapes, the `/experiments` fingerprint, field error codes other than INVALID_LOGIN / ACCOUNT_LOGIN_VERIFICATION_EMAIL, and the body-vs-header captcha retry are from knowledge of the web client, not observed. Tests use canned JSON and a local mock HTTP server (`AuthClient::with_base("http://127.0.0.1:…")`; only loopback may be plain HTTP).

## Jumping to messages beyond the loaded history (2026-11-20)

- Clicking a reply's quote now uses `jump_or_fetch` (as search, pins and notifications did): a message outside the window loads the page around it (`?around=`).
- That page replaces the channel's window and is marked `detached` (`MessageBuffer::replace_around`) when its last message is older than the channel's `last_message_id`. A detached window takes no live MESSAGE_CREATEs, so it never has a gap; the channel's last id and unread state still update.
- Scrolling within 400 px of the bottom of a detached window fetches `?after=` the newest row (`load_newer`); a short page means it reached the present and live messages are added again.
- A "Jump to Present" pill shows over the list while detached. It, and sending a message from a detached view (`follow_sent_message`), drop the window at once (so messages arriving meanwhile are kept), fetch the newest page and rebuild.
- Before, `jump_or_fetch` replaced the window with an old page and then appended live messages after it, leaving a silent gap and no way back down.
- Unverified on screen: the demo has no REST, so the fetches were not exercised; reducers are unit-tested.

## Group DMs (group_dm.rs)
- DM list header has a `+` (Create DM) → `group_dm_dialog(None)`: an `adw::Dialog` with search (name and username), a checklist of friends (`relationships_of(FRIEND)`), removable chips for the picks, "You can add N more friends." and a Create button. One friend → `open_dm_with` (reuses an existing DM); several → `POST /users/@me/channels {"recipients": [...]}` (`create_group_dm`), the response applied as CHANNEL_CREATE (the gateway's copy then just updates the same entry) and opened.
- Cap: 10 people including you (`MAX_GROUP_SIZE`); when full, unticked rows go insensitive and `toggle_pick` refuses more. Adding to a group counts its recipients + you.
- Group DM row right-click (and the member panel's "Add Friends to Group" button): add friends (`PUT /channels/{id}/recipients/{user}` each, applied locally as CHANNEL_RECIPIENT_ADD), rename (`PATCH /channels/{id} {"name"}`, empty → null = members' names), leave (`DELETE /channels/{id}` after an AlertDialog; applied as CHANNEL_DELETE, goes to Friends if open). The member menu shows "Remove From Group" (`DELETE /channels/{id}/recipients/{user}`) in a group you own.
- CHANNEL_RECIPIENT_ADD/REMOVE were already handled by the reducer (replace-by-id, so local + gateway copies don't duplicate).
- None of these endpoints has been exercised against Discord (no live writes); bodies are unit-tested. `KUBECORD_DEMO=group-create|group-picked|group-search|group-full|group-add|group-menu|group-members|group-rename|group-leave` shows the UI offline (the demo now has ten more friends and a group DM you own).

## DM list: hover close button and full-status tooltip (2026-11-20)

- Each DM row has a small circular `window-close-symbolic` button (`kc-dm-close`), shown only while the row is hovered (CSS `button.kc-channel:hover`), with `focus-on-click` off so its click does not select the row. A 1:1 DM closes (`close_private_channel`, DELETE /channels/{id}); a group asks first (`confirm_leave_group`). Demo: `KUBECORD_DEMO=dm-hover|dm-leave|dm-deleted`.
- The DM row's status/subtitle line is ellipsised, so it now carries a tooltip of the full `row.subtitle` (the presence summary). The demo account has no custom status, so the tooltip text itself was not shown in a screenshot.

## Stream volume separate from voice; stopping a watch (2026-11-20)

- A person's screen-share sound and their voice now have separate playback volumes. The mixer (`kc_media::receive`) already keyed stream audio apart (`stream_audio_key(user_id) = id | 1<<63`); what was added is a second stored value and a second UI control. `session.rs`: `VoiceRuntime.stream_volumes` beside `user_volumes`; `SetUserVolume` now applies only to `user_id.get()`, new `SetStreamVolume` applies only to `stream_audio_key`. Both maps are re-applied on (re)connect; a watched stream's saved volume is set on its first audio packet (new mixer entries default to 100%).
- UI: the user menu gains a "Stream Volume" slider beside "User Volume" while the person is screen-sharing (`ctx.streaming`). Both built by one `playback_slider` helper in `menu.rs` on the same scale (Features → Maximum User Volume cap). `AppState.stream_volumes` mirrors the chosen values so the slider reopens at them, like `user_volumes`. Neither map is persisted to disk — same as before.
- Stop Watching: already had `Command/VoiceCommand::StopWatching` and a button on the enlarged (Stage) stream tile. Added a "Stop Watching" user-menu item (inverse of "Watch Stream", shown when `ctx.watching`) and a button in the stream pop-out window beside its info control. Stopping clears `voice.watching`, so the tile returns to `NotWatching` (a "Watch Stream" button).
- Verified on screen: `KUBECORD_DEMO=stream-menu` (the menu, with both sliders at 100% and Stop Watching) and `KUBECORD_VIDEO_DEMO=focus` (Stop Watching on the enlarged tile). The pop-out's new button is hover-revealed (CSS `.kc-popout:hover .kc-tile-controls`) so, like the pre-existing pop-out info button, it is not captured in a static screenshot; `KUBECORD_VIDEO_DEMO=popout` now saves the pop-out to `<screenshot>.popout.png` but shows only the video. Tests: mixer independence and the command→state mapping. Not verified live: no second account to watch, and watching/stopping and the two volumes against a real call are untested (voice-gateway/RTP actions).

## Optimistic (pending) message sending (2026-11-20)

- Sending a message now shows it immediately as a greyed row with a spinner and "Sending…", replaced in place by Discord's confirmation. A failed send stays as a row with "Couldn't send." + Retry / Delete.
- `Message` gained `#[serde(skip)] send_state: SendState` (Sent/Sending/Failed; Sent is the serde default, so every gateway message is Sent). `Message::pending(id, channel, author, content, nonce)` builds one.
- The temp id is `Snowflake::from_unix_millis(now, counter)` (kc-core), which sorts after every real message so the row sits at the bottom. The nonce is that id's value as a string; Discord echoes it, and `message_create` matches it to drop the pending row (`MessageBuffer::take_pending`) before inserting the real one. Idempotent: applying the HTTP response and the gateway echo both reconcile.
- `Store`: `add_pending` (skips a detached window), `set_send_state`, `drop_pending`. `nonce_str` reads a nonce as a string whether Discord sent it as a string or a number.
- Composer: `dispatch_send(channel, OutgoingDraft)` is the one send path (normal + retry). It adds the pending row, sends with `Outgoing.nonce`, and on failure marks it Failed. `OutgoingDraft` (content/reply/mention/uploads) is kept in `ChatView.pending_sends` by nonce so Retry re-sends (uploads are `Arc`, cheap to keep). Optimistic is skipped for attachments-only sends (empty bubble) and while the view is on older messages (it jumps to present instead). Demo: `KUBECORD_DEMO=chat-sending`.
- Recovery note: this work was briefly lost when a parallel agent stashed the shared tree; it was restored from `stash@{1}`/`stash@{0}`. When two agents share a tree, commit often and never `git stash`/`reset` the whole tree.

## Diagnosing a periodic app-wide hitch / DM hover flicker (2026-11-20)

- User reports: a slight hitch across the whole app "every so often", DM hover highlight flickering off/on, and the same stall affecting a running screencast — "all at once", not isolated to one area. A global, periodic main-thread stall.
- Confirmed mechanism for the DM flicker: `queue_sidebar` rebuilds the guild rail + channel list with `clear_box` (full teardown), destroying the hovered row so its PRELIGHT resets. It is queued on frequent events: DM-mode `PresenceUpdated`, `ReadStateUpdated` (every message anywhere), `VoiceStateUpdated`, `SettingsUpdated`. `PresenceUpdated` is already gated to DM mode (no rebuild while viewing a guild channel).
- Not yet pinned: the *app-wide* stall's single cause (candidates: the member-list rebuild `render_members`, a burst of store events, or sidebar rebuilds coinciding). Rather than guess, added measurement (lesson: measure the failing run first).
- Instrumentation added in `app.rs`: the UI event loop times each `handle_event` and logs `WARN "a UI event blocked the main thread" event=<kind> millis=<n>` at ≥16 ms; a 100 ms watchdog timer logs `WARN "main thread stalled" millis=<n>` at ≥250 ms. Labels are event-kind only (`event_label`/`store_event_label`), never payloads, so the log carries no private data. Next step: have the user reproduce and read which `event=` dominates.

## Boosting the user's own outgoing screen-share audio (2026-11-19)

- Feature: a "Shared Audio Boost" that makes viewers hear the user's shared stream sound louder, WITHOUT changing the user's own output/monitor volume. It scales the outgoing PCM in the stream's monitor tap, which is a separate capture (`SharedSound` records kubecord's `kubecord Go Live` virtual output), so the user's playback is untouched.
- `kc-media::audio::ScreenAudio` gained a `gain: Arc<AtomicU32>` (f32 bits) and `set_gain(f32)`; `ScreenAudio::start` takes a `gain` param (callers pass the saved value). `on_frame` calls `crate::devices::apply_gain(&mut frame, gain)` before Opus-encoding. `apply_gain` already existed (used for mic/speaker gain): scales `sample as f32 * gain` and clamps to i16 (saturates, never wraps); unity returns early. Tests in `devices.rs`: `unity_gain_leaves_samples_unchanged`, `large_boost_clips_to_the_rails_not_wraps`, plus the pre-existing `gain_scales_and_saturates` (0 = silence). Live-adjustable: the atomic is read per frame, so `set_gain` takes effect on the next 20 ms frame without restarting the stream.
- Plumbing (mirrors `SetStreamVolume`): new `Command::SetStreamAudioGain { account, gain }` → `VoiceCommand::SetStreamAudioGain { gain }`. The voice handler stores it on `VoiceRuntime.stream_audio_gain` (default 1.0) and, if a stream connection with audio is up, calls `audio.set_gain(gain)`. `ScreenAudio::start` (in `VoiceControl::StreamSource`) reads `voice.stream_audio_gain` so a stream that starts later begins at the saved boost. Test: `stream_audio_gain_is_kept_on_the_runtime`.
- Setting: `CapturePreferences.stream_audio_gain: f32` (default 1.0; the struct's `#[serde(default)]` fills it from `Default` so old settings files read as 1.0 / 100 %). UI control is in the Go Live popup's Stream Audio group (`video_setup.rs::stream_audio_controls`, now takes `account`): a `gtk::Scale` 100 %–500 % (gain 1.0–5.0) with a 100 % mark, default 100 %, and a subtitle noting it boosts what viewers hear (not the user's own volume) and that too much distorts/clips. Moving it `update_prefs` (persist) and sends `SetStreamAudioGain` live. The Go Live action also sends the saved gain before `StartStream`/`StartStreamCamera`, so the runtime has it even when the slider was not touched.
- Verified: `cargo fmt --check`, `clippy -D warnings`, `cargo test --workspace` (incl. the 3 gain tests + the runtime mapping test), `nix build` all green. Screenshot of the new control offline: `KUBECORD_POPUP=share KUBECORD_SCREENSHOT=<path>` (the share popup is an in-window dialog, so the main-window PNG catches it; a save hook was added to the no-account `share` branch in `app.rs`). The slider, 100 % mark/readout and subtitle render correctly.
- NOT verified live: no second account / real viewer, so the actual loudness viewers hear and whether a large boost audibly clips are unconfirmed against Discord. No live account actions were taken.

## "Full frames: N s ago" jittered (2026-11-20)

- The video info panel's "N s ago" was `stats.since_keyframe / arriving_fps`, where `arriving_fps` is an instantaneous estimate over the last ~1 s sample. The frame count since the keyframe climbs steadily but the fps estimate wobbles, so the quotient jumped (e.g. 39 → 37 → 39 → 40) instead of only counting up.
- Fix: `refresh_video_info` now records the `Instant` the keyframe count last increased (`Sample.keyframe_at`, carried forward until `stats.keyframes` goes up) and passes real elapsed seconds to `describe`; `describe` no longer divides by fps. The value now only climbs and resets to ~0 (within the 1 s sample granularity) when a new full frame arrives. `arriving` was removed as unused.

## Hitch diagnosis, round 2 (2026-11-20)

- The user's log (instrumented build e0fb4b2): 140× "main thread stalled" 250–300 ms in dense bursts, ZERO "a UI event blocked the main thread". Stalls begin only after joining a call with video (clean while idle/connected). So it is not kc-ui event code and not the sidebar; it is tied to in-call video. Stream was 720p30 at 17–19 Mbit/s, hardware VA-API. The self-preview re-decodes the own stream (avdec_h264) — `LocalPreview::push` feeds encoded frames to a `VideoFeed`/`VideoDecoder` into a gtk4paintablesink.
- Switching to the software (x264) encoder did NOT help → not GPU/VA-API encoder contention.
- Remaining hypotheses: whole-process CPU starvation (x264/avdec saturating cores), the GTK renderer repainting the window at frame rate, or main-thread lock contention (e.g. the store RwLock held by a background thread during big payloads).
- Added an off-thread watchdog (`kc-watchdog` std::thread, 100 ms sleep, warns ≥250 ms "off-thread watchdog stalled") alongside the main-loop one. If both stall together → process-wide CPU starvation; if only the main-loop watchdog → main thread busy/blocked. Next: user reproduces, compare the two warnings in the log.

## Hitch diagnosis, round 3 — it's the sidebar/member rebuild on a big account (2026-11-20)

- Decisive clue from the user: the hitch happens ONLY on their large account (many servers + DMs), not a small one, and with NO screenshare. So it is not video — the earlier "in-call only" was incidental (a call just generates more store events: voice states, presence).
- Why the event timer missed it: `queue_sidebar` rebuilds the guild rail + channel list in a `glib::idle_add_local_once` callback, and `render_members` runs from event arms — but the heavy sidebar rebuild is in that idle callback, OUTSIDE `handle_event`, so the per-event timer never covered it. The watchdog (outside the event loop) did catch it as "main thread stalled".
- The rebuild is a full `clear_box` + recreate of every guild tile and channel/DM row; its cost scales with account size, and it is queued on frequent events (DM-mode presence, read-state on every message anywhere, voice state, settings). Big account × frequent triggers = the bursts of 250–300 ms stalls, and the DM hover flicker (the hovered row is destroyed and remade).
- This round: `queue_sidebar` now coalesces over an 80 ms timeout (was next-idle) so event bursts collapse into one rebuild, and both the sidebar rebuild and `render_members` are timed (WARN "rebuilding the sidebar/member list blocked the main thread" millis=…). Next: confirm the per-rebuild cost from the user's log, then the real fix is to stop full-rebuilding (rebuild the rail only on guild/unread changes, the channel list only for relevant changes, and update DM status/unread in place instead of tearing down the list).

## Rail rebuild skipped when unchanged (2026-11-20)

- Confirmed from the user's log: one sidebar rebuild blocks the main thread up to 174 ms on their large account (many servers); the off-thread watchdog never stalled, so it is main-thread rebuild work, not CPU starvation; `render_members` never exceeded 16 ms. So it is the guild rail + channel list rebuild.
- `render_guilds` now fingerprints its snapshot (the `entries` rail model + the unread-DM shortcuts by id/title/icon/count + selected_guild/dm_mode/open_folders) and returns early when it matches the last render (`rail_fingerprint` on AppState), instead of recreating an avatar widget per server. Most triggers (read-state on a message in some server, presence of someone not shown) do not change the rail, so they now skip it. The rail is a pure function of that data, so skipping an identical render is safe.
- Still rebuilds unconditionally: the channel list (`render_channels`/`fill_channels` → `render_dm_list`/`render_guild_channels`). If the user's post-fix log still shows "rebuilding the sidebar blocked …" with high millis, that residual is the channel list; fingerprint it the same way, but hash only display-relevant fields (NOT the whole `Channel`, whose `last_message_id` churns on every message).

## Channel list rebuild skipped when unchanged (2026-11-20)

- After the rail skip, the user's log still showed "rebuilding the sidebar blocked the main thread" ~130–146 ms — that residual is `render_channels` (the DM list / open server's channels), which rebuilt unconditionally.
- `render_dm_list` and `render_guild_channels` now fingerprint their model and skip the rebuild when it would draw the same list (`channels_fingerprint` on AppState). The clear (`clear_box` + `voice_rows.clear()`) moved out of `fill_channels` into each builder, after its gate, so a skip leaves the list, its scroll position and hover untouched.
- The fingerprint hashes only displayed fields (`hash_channel_row`, and the DM row fields), NOT the raw `Channel` — `Channel::last_message_id` changes on every message without changing the row's look, so hashing it would defeat the skip. Includes a mode tag ("dm"/"guild"), guild_id, `selected`, collapsed categories, and incoming-request count, so switching lists or selection always rebuilds. `VoiceUser` derives `Hash`.
- Verified: fmt/clippy/`cargo test --workspace` (335) and `nix build` green; demo screenshots of the guild channel list and the DM list both render.

## Rail rebuild made cheap by reusing tiles (2026-11-20)

- Measured locally with `KUBECORD_DEMO_GUILDS=N` (adds N iconned servers to the demo READY) + a scratch timer: the rail rebuild (`render_guilds`) scales linearly with server count and dominates; the channel list is ~2 ms. Recreating an `adw::Avatar` + Button + Overlay + gestures per server is the cost (150 servers ≈ 45 ms with no image; more in reality, since cached avatars also run `set_custom_image`).
- Fix: `cached_guild_slot` memoizes each guild tile by a hash of its drawn state (name, icon, unread, mentions, selected) in `AppState::guild_slots`. A rebuild (triggered by one server's unread changing) reuses the other tiles — and their already-loaded avatars — instead of recreating them; only changed/added tiles are built. `clear_box` unparents the cached tiles first, so a reused one re-appends cleanly; stale ids are evicted each rebuild. Measured 150 servers 45 → 14 ms, and in reality more since reused tiles skip `set_custom_image` entirely.
- Remaining cost is `clear_box` + re-append of all tiles (GTK reparenting), ~0.1 ms/server. A true in-place update (no reparent when order is unchanged) would cut it further but needs widget-order diffing; deferred.
- The rail rebuild timing is now split (`rail_ms` / `channels_ms`) in the queue_sidebar log line.

## Performance diagnostics behind a setting (2026-11-20)

- The main-thread watchdogs (main-loop + off-thread), the per-event timer, and the sidebar/member rebuild timings now log only when `crate::logging::diagnostics()` is on (checked before building the warning, so free when off).
- New setting `CapturePreferences.log_diagnostics` (serde default false), applied via `crate::logging::set_diagnostics` in `apply()`, with a "Performance Diagnostics" switch in Settings → Advanced → Log File. Off by default, so release logs stay clean; turn on to investigate a hitch.

## Release correctness review fixes (2026-11-20)

Four read-only review agents went over the session's untested code. No high-severity bugs. Fixed:
- notify.rs: a muted server/channel still notified for role and @everyone/@here pings — a muted place now notifies only for a direct @mention (gated the role/everyone branches on `!muted`). Test extended.
- group_dm.rs: "Add Friends to Group" applied the optimistic CHANNEL_RECIPIENT_ADD for every selected user even when the HTTP add failed; now it applies only the ids the server accepted (stops at the first failure, reports the rest).
- menu.rs + server_settings.rs: the Server Settings members list moderated `session.selected_guild`, not the dialog's guild — so switching servers behind the (non-modal) settings window could kick/ban on the wrong server. Added `show_user_menu_in`/`attach_user_menu_in` taking an explicit guild; the members list passes `panel.guild`.
- sounds.rs: the ring's 60 s auto-stop backstop used one shared serial, so a concurrent opposite-direction ring defeated it; now one serial per direction. Also dedup script URLs with a set (not adjacent-only) before the priority sort.
- server_settings.rs: removed the dead `filling` guard (never set; the set-before-connect order already prevents spurious patches).

Left (low / uncertain, noted for the user): login Cancel during an in-flight request can still add the account (edge case); SMS-send captcha not routed through the solve flow (rare); a late STREAM_DELETE after a gateway-session rejoin could tear down a re-created stream (timing-dependent).

## Login edge fixes + LICENSE (2026-11-20)

- Login Cancel/restart race: `LoginPage.generation` is bumped in `reset()`; each request captures it and a stale result (from a cancelled or superseded attempt) is dropped (`login_answered_if_current`, and the SMS-send callback checks it). So backing out of a login no longer signs you in when the in-flight request returns.
- SMS-send captcha: `AuthClient::send_sms` now takes a captcha answer and returns `Error::Captcha` on a captcha response (via the same path as `/auth/login`); `sms_send_body` carries the captcha key. The UI's `send_login_sms` goes through `with_captcha`, so a captcha on "Text Me a Code" is solved in-app and retried instead of dead-ending. Test covers the captcha body.
- Added an MIT `LICENSE` (copyright kyleraykbs, 2026) — swap for another license if wanted; version stays 0.1.0.

## Edit Profile (user + server) and account Log Out (2026-11-20)

- Profile editor (`members.rs::edit_profile_dialog`, opened from the own-profile popup's Edit Profile button and Settings → My Account) is now an `adw::ViewStack` with an `InlineViewSwitcher`: a User Profile tab and a Server Profile tab, with one shared Cancel/Save row. Save routes by the visible tab (`save_user_profile` / `save_server_profile`).
- Avatar editing: `avatar_field` builds a 64 px preview with Change (a `FileDialog` image pick → `spawn_local` reads the bytes, refuses > 10 MB, previews via `gdk::Texture::from_bytes`, keeps the pending `AvatarEdit::Set`) and Remove (`AvatarEdit::Remove`, shows initials again). The pending edit lives in an `Rc<RefCell<AvatarEdit>>` read on save. Picking must use `spawn_local` not `spawn`: the done-callback touches GTK widgets and is not `Send`.
- User tab saves display name (`set_global_name`), bio+pronouns (`set_profile`) and avatar (new `HttpClient::set_avatar(Option<&[u8]>)` → `PATCH /users/@me` with a data URI, or `{"avatar": null}` to remove). Server tab saves nickname (`set_nickname(guild, me_id, true, nick)` — the `@me` member route) and bio/pronouns/avatar via the new `set_server_profile` (`PATCH /guilds/{id}/profile/@me`) built from `ServerProfileEdit` (nick/bio/pronouns `None` = untouched, empty string = clear; `AvatarEdit` Keep/Remove/Set). Server picker defaults to the open guild; nick prefills from state, bio/pronouns from the member profile fetch.
- kc-http: `AvatarEdit`, `ServerProfileEdit`, `avatar_body`, `set_avatar`, `set_server_profile`; payload bodies unit-tested (data URI, null remove, server-profile keep/clear/set).
- Account Log Out: each row in the account menu (`app.rs::render_account_controls`) has a `system-log-out-symbolic` button → `confirm_log_out` (an in-window `adw::AlertDialog`, destructive, since it drops the saved token) → `log_out_account`. That removes the session from `model.sessions`, removes the account from the store (`AccountStore::remove_by_token`, which handles pre-READY accounts with no `user_id`; the token is only used to identify, never printed), stops the running loop, and if it was active switches to another running session or shows the login page.
- Clean teardown: new `Command::StopAccount { account }` removes the account's entries from `VOICE_COMMANDS` (dropping the only long-lived `voice_tx`, so the account loop's `voice_rx.recv()` returns None and the loop ends) and `GATEWAYS`. The loop's exit now runs `teardown_voice` (ends call/stream) and then `gateway_task.abort()` — the gateway reconnects on its own after a socket drop, so dropping the handle is not enough; the stored `JoinHandle` (was `_gateway_task`) is aborted so no socket/heartbeat is left. Dev aid `KUBECORD_DEMO=accounts|accounts-logout|accounts-done` (the menu's Log Out buttons, the confirmation, and an actual logout that prints `demo: after logout sessions=.. accounts=.. page=..`). `accounts-done` shows sessions=0 accounts=1 page=login.
- NOT verified live (constraint: no real account writes): no avatar/profile/nick PATCH or server-profile PATCH was sent to Discord, and no real account was logged out. Only the payloads/removal logic are unit-tested and the UI is screenshotted offline. Per-server avatar/bio/pronouns beyond nickname are built and payload-tested but their live acceptance by Discord is unconfirmed.

## "Ringing …" banner on an outgoing call (2026-11-20)

- When you start a DM/group call, `start_call_with` rings the others and sets the outgoing ring (`calling_channel()`). `build_call_view` now shows a "Ringing <names>…" pill (`ringing_banner`, spinner + the channel's recipients not yet in the call) above the tiles while `calling_channel() == channel_id`.
- It clears when the ring stops: the `CallRinging { anyone: false }` handler now calls `refresh_voice_view()` after `call_stopped_ringing()`, and a recipient answering re-renders the view (they move into `people`, so they drop out of the banner; when all have answered, `anyone` is false and the banner goes).
- Screenshot-unverified: in demo mode sounds don't play so the calling slot isn't set; the logic compiles and is straightforward (a spinner + label in a `kc-ringing-banner` pill). Verify on a real outgoing call.

## Unlabeled accounts: filler text, then the name saved as the label (2026-11-20)

- `account_name` (app.rs) no longer falls back to the redacted token for an account with no label and no live session yet; it shows "Signing in…" instead (the token is never shown in the list).
- On `UiEvent::Ready`, if the account's label is empty/None it is set to the user's `display_name()` and saved (`model.accounts.upsert` + `save`). So a token-only account is named by its user once, permanently, and the filler is only ever seen during the brief first connect.

## Diagnosing MFA code rejection (2026-11-20)

- User: email+password login succeeds (`/auth/login` 200), but BOTH `/auth/mfa/totp` and `/auth/mfa/sms` return 400 ("invalid code"); `/auth/mfa/sms/send` succeeds (200). So it is not clock skew (SMS is not time-based) and not the ticket (sms/send used it fine) — it is specific to the code-submit request (`submit_code`/`mfa_body`).
- Added safe diagnostics in kc-http `auth.rs` (no secrets): `send` logs Discord's error `code`/`message`/`fields` on any failure (`WARN login request refused`); `submit_code` logs the code's length + digit count + ticket length + method (`INFO submitting a two-factor code`), never the code or ticket value. Next: user retries, and these two lines say whether the code is mangled/empty (field-read bug) or clean (a request/API issue — then the error code/message names it).

## MFA code rejected: missing device cookies (2026-11-20)

- Diagnostics confirmed: the code reaches `/auth/mfa/totp` as a clean 6 digits with a valid 70-char ticket (the one `sms/send` accepted), the system clock is NTP-synced, and Discord returns a real `60008 "Invalid two-factor code"` for BOTH TOTP and SMS. So not the code, ticket, or clock — the request itself is distrusted.
- Cause: `AuthClient`'s reqwest client had no cookie store, so Discord's device cookies (`__dcfduid`/`__sdcfduid`, set on the first request and tied to the fingerprint) were discarded and never sent on the code submit. Discord issues the ticket but rejects the actual code verification without them — the same as a browser with cookies blocked, which is why login and `sms/send` pass but the verify fails.
- Fix: `reqwest` "cookies" feature added (workspace Cargo.toml), and `AuthClient::with_base` now builds the client with `.cookie_store(true)`, so the cookies from the lazy `GET /experiments` (fingerprint fetch, the first request) carry through login → mfa. Unverified live (needs the user's next login attempt); the refused-request diagnostics stay in to confirm.

## Unlimited scroll-back, jump-to-latest button, sliding window (2026-11-20)

- The store capped each channel at 300 messages and `extend` (scroll-back) pruned the OLDEST — so loading an older page past 300 dropped the page just loaded. Scroll-back hard-stopped at 300.
- Window raised to `MESSAGE_WINDOW = 2000` and the prune direction fixed: `extend` no longer caps; callers cap the correct end — `cap_oldest` (drop the top) for live `insert`/`extend_newer`, `cap_newest` (drop the bottom, set `detached`) for `State::load_history` (scroll-back). Gated by `UNLOAD_DISTANT` (`set_unload_distant`, default on). With it on the window slides: scroll back forever, the distant newest unload and reload on the way down (reusing the detached-window machinery). With it off nothing is dropped (everything scrolled into view stays).
- Setting `CapturePreferences.unload_distant_messages` (serde default true via `default_true`), applied through `kc_state::set_unload_distant`, with a Chat → History → "Unload Distant Messages" switch.
- `chat.rs`: the old "Jump to Present" overlay is now a round go-bottom pill shown whenever scrolled up (`update_present` on `!at_bottom || detached`); clicking scrolls to the bottom, or reloads the newest page when detached. `history_loaded` calls `trim_to_store` (synchronously, before the scroll-restore anchor) to drop UI rows the store unloaded, keeping the view and store in step and actually freeing the widgets.
- Test `the_window_caps_the_correct_end_and_can_be_turned_off`. Verified on screen: not yet (the demo tree is mid-edit by another agent); the logic is unit-tested and the jump button is a standard overlay.

## Animated GIFs, favourite GIFs, file downloads (gif/favorites/download)
- Real animated `.gif` files rendered frozen: images decode to a single `gdk::Texture` (`images.rs`). New `animation.rs`: `decode` (bytes → `PixbufAnimation` via `MemoryInputStream`, `None` for a still), `GifPlayer::attach` drives a `gtk::Picture` by advancing a live `PixbufAnimationIter` on a `glib::timeout` against a virtual clock, repainting from `MemoryTexture` of each frame. Runs only while mapped (offscreen gifs stop), owned by the picture (dropped with it). `images.rs set_animated_picture` downloads via the disk cache (`file(url,"gif",…)`), animates `image/gif` bytes, else falls back to a still texture. Discord's `gifv` embeds (MP4s) keep the unchanged GStreamer `LoopingVideo` path.
- Applied to: message `image/gif` attachments (`content.rs`, new `MediaKind::AnimatedGif`), the GIF picker tiles (`composer.rs`, cap `GIF_PICKER_ANIMATED=16` animated tiles, rest still), and the image viewer (`image_viewer.rs`, `.gif` URL → animated; zoom/pan unchanged). Clicking a real gif attachment opens it in the viewer (item: click-to-open); a `gifv` keeps playing inline (its MP4 can't animate as a pixbuf in the viewer).
- GdkPixbuf gotchas learned the hard way: a GIF's Graphic Control Extension must be a full 8 bytes ending in a block terminator, and LZW code width steps up to 4 bits once the dictionary hits 8 entries — a 1-literal frame decodes as "truncated". `on_currently_loading_frame` is true for the LAST frame of a finished animation, so it can't be a frame-enumeration guard; drive the live iterator instead. Test builds a 2-frame 3x1 GIF by hand and checks it advances through distinct frames with delays.
- Favourite GIFs: `HttpClient::favorite_gifs()` (read-only `GET /users/@me/settings-proto/2`) base64-decodes the `FrecencyUserSettings` protobuf and reads `favorite_gifs` (field 1) → `gifs` map<string,FavoriteGif> (FavoriteGif: format 1 [2=image,3=video], src 2, width 3, height 4, order 5). Hand-rolled varint/length-delimited walker in `favorites.rs` (no prost/build step); fails soft to empty on any error. `Gif` gained a `video` flag so a favourite video plays as a gifv. Picker's empty-search view now shows a Favorites section then Trending (labelled sections like the emoji picker); search shows Results. Parser unit-tested against a hand-built base64 blob. Not verified live (no account exercised).
- File attachments (`content.rs attachment_widget` kc-file card): added a right-side Download button (`folder-download-symbolic`/`document-save-symbolic`), name/size left-aligned, text column `hexpand`. Click → `FileDialog::save` (initial name = filename) → `images.file` download → `gio::File::copy` to the chosen path → `set_status` Saved/Could not. Filename link unchanged.
- Demo aids (`demo.rs`): `KUBECORD_DEMO=gif` (message attachment), `gif-viewer` (viewer, saved `<shot>.viewer.png`), `gif-picker` (Favorites+Trending sections, saved `<shot>.menu.png`), `file` (file card). GIF URLs point at a public Wikipedia GIF so a still/first frame shows offline; motion needs the network. The demo REST points nowhere, so real favourites/trending/search never load there.

## Ringing banner: vibrating phone icon (2026-11-20)

- The outgoing-call "Ringing …" banner now shows a `call-start-symbolic` phone icon (class `kc-ringing-phone`, success-green) instead of a spinner. CSS `@keyframes kc-phone-vibrate` animates `-gtk-icon-transform: rotate()` in a shake burst then rest (1.3 s loop), like a vibrating phone. `-gtk-icon-transform` is the GTK4 way to transform an icon and is animatable; verified the CSS parses with no theme error.

## GIF picker: Favorited / Search tabs (2026-11-20)

- Replaced the Favorites-then-Trending sections with two tabs (linked ToggleButtons, `GIF_TAB`): **Favorited** (default) and **Search**. Opening the picker lands on Favorited.
- Favorited tab: favourites are fetched once per opening and cached (`GIF_FAVORITES`, cleared on each `connect_show`); typing filters the cache by the Tenor URL slug (favourites carry no title). Placeholder "Search your favorites".
- Search tab: empty shows Trending; a query runs `gif_search` over all of Tenor. Placeholder "Search Tenor".
- One shared flow (`gif_flow`/`fill_gif_flow`, `show_gifs`) replaces the multi-section layout; switching tab re-runs the current search against that tab's source. Verified with `KUBECORD_DEMO=gif-picker` (tabs + favourite tiles render).

## Jump button, viewer zoom, demo sign-out dialog (2026-11-20)

- Jump-to-latest button lingered: visibility only refreshed when `at_bottom` toggled, but `scroll_to_bottom` pre-sets `at_bottom`, so reaching the bottom (and clicking the button) often didn't hide it. Now `update_present` runs on every scroll event (short-circuited: the store `detached` read only happens when already at the bottom) and `scroll_to_bottom` calls it directly.
- Image viewer zoom stretched vertically at some zooms: with `ContentFit::Fill` + a size request, the ScrolledWindow stretched the picture to the viewport in the non-scrolling dimension (a wide image zoomed shorter than the viewport → vertical stretch). Fixed by centering the picture when zoomed (`halign`/`valign` Center) so it gets exactly its requested size; reset to Fill when fitted.
- Offline demos showed a real "Discord Signed You Out" dialog (for the demo account): `start_existing` started real accounts from a leftover sandbox `accounts.json`, whose token Discord rejects (4004). Activate now skips `start_existing` when `KUBECORD_DEMO`/`KUBECORD_VIDEO_DEMO` is set (the demo brings its own session); `KUBECORD_POPUP` still uses a real account.

## Setting: animate GIFs while unfocused (2026-11-20)

- Real GIFs (`GifPlayer`) previously animated whenever mapped, ignoring focus; gifv videos paused when unfocused. Now both pause when the window is unfocused by default, kept in sync by a global `WINDOW_FOCUSED` (set from the window's `is-active` handler) and `ANIMATE_UNFOCUSED` setting.
- `animation.rs`: `awake() = WINDOW_FOCUSED || ANIMATE_UNFOCUSED`; a `PLAYERS` registry of live `GifPlayer`s; `set_window_focused`/`set_animate_unfocused` refresh them all; `GifPlayer::refresh` runs the timer only when mapped && awake, and `tick` stops instead of rescheduling when not awake. `player.rs` gifv gate is now `is_active() || animate_unfocused()`.
- Setting `CapturePreferences.animate_gifs_unfocused` (serde default false), applied in `apply_devices` via `animation::set_animate_unfocused` (so it takes effect live from `update_prefs`), with a Chat → GIFs → "Animate GIFs While Unfocused" switch (off by default). The toggle also calls set directly for immediacy.

## Favourite GIFs: star button + proto writer (2026-11-20)

- Write side (`kc-http/src/favorites.rs`): added a hand-rolled protobuf *writer* beside the reader. `add_favorite`/`remove_favorite` round-trip the `FrecencyUserSettings` blob by splitting the top-level message into ordered `(field, wire, raw-value)` spans (`split_fields`), keeping every field's bytes verbatim, and only re-encoding `favorite_gifs` (field 1): its own non-`gifs` fields stay raw too, only the `gifs` map (field 1 entries) changes. Add computes `order` as max+1 and de-dupes by url (map set semantics); remove drops the entry whose key matches. Malformed input returns `None` (caller refuses to PATCH rather than clobber settings). `HttpClient::set_favorite_gif`/`favorite_gif`/`unfavorite_gif` GET the proto, apply, base64, `PATCH /users/@me/settings-proto/2`. New `Error::Other(String)`; public `FAVORITE_GIF_FORMAT_IMAGE=2`/`_VIDEO=3`.
- Safety test `favouriting_adds_an_entry_and_preserves_everything_else`: builds a blob with favorite_gifs (2 gifs + a nested unknown field 9) plus top-level unknown varint field 5, unrelated field 7, and unknown LEN field 9; favourites a new gif → asserts 3 gifs with right key/src/format/order, every other top-level field byte-identical, nested unknown survived; then unfavourites → back to 2, others still identical. Plus dedupe, empty-proto-creates-favorite_gifs, missing-url-noop (byte-identical), malformed-refuses. **The live PATCH is unverified against the real account.**
- Shared cache (`kc-ui/src/gif_favorites.rs`): thread-local `Vec<Gif>` cache used by both picker and chat. `is_favorited(url, src)` matches a cached gif by either `url` or `src`. `set_star_icon` picks filled (`starred-symbolic`) vs outline (`non-starred-symbolic`), falls back to ★/☆ labels, toggles a `kc-favorited` class (keeps filled stars visible without hover). The cache is fetched once on `UiEvent::Ready` (`prefetch_gif_favorites`, re-renders messages) and refreshed on each picker opening. **Caution:** the picker fetch must run in `done` (GTK thread), not the spawned future (runtime worker thread) — thread-locals are per-thread. `spawn`'s `done` is `Send` so it can't capture widgets; the star toggle uses `video_setup::spawn_local` (runs `done` on the GTK thread) so it can flip the button icon.
- UI: `gif_star`/`toggle_gif_favorite` (composer.rs) build the star and toggle optimistically (flip icon + cache, revert on error). Picker tiles (`fill_gif_flow`) wrap the send button in a `gtk::Overlay` with the star top-right. Chat gifs (`content.rs media_preview`) add the star to the existing overlay when `Media.favorite: Some(FavoriteTarget)` is set — real-gif attachments (format IMAGE, key = attachment.url) and gifv embeds (format VIDEO, key = original non-proxy media url). CSS `button.kc-gif-star` (style.rs): opacity 0, revealed by `overlay:hover`, gold when `.kc-favorited`.
- Demo: `KUBECORD_DEMO=gif` now seeds the favourites cache so the chat GIF's star shows filled (verified: gold star top-right of the GIF message). `gif-picker` tiles render the star too (6 tiles confirmed built), but offline the tile `Picture`s have no paintable so a popover snapshot shows empty tiles — the star structure is identical to the chat path. Screenshots: `KUBECORD_SCREENSHOT=<path>` saves to `<path>` verbatim (no `.png` suffix for the main window; `.menu.png` for popovers).

## Favorites tab empty: wrong proto field number (2026-11-20)

- The favourites parser/writer put `favorite_gifs` at field 1 of `FrecencyUserSettings`, so it never found any (the tab was empty). Confirmed against the user's live settings-proto/2 (read-only dump of field structure, no values): field 1 is `versions` (4 bytes); `favorite_gifs` is **field 2** — its values are exactly `FavoriteGif` (format/src/width/height/order) keyed by URL (the account had 3). Other frecency maps: fields 6, 9, 13.
- Fixed to `FAVORITE_GIFS_FIELD = 2` in `favorites.rs` reader (`parse_settings_proto`) and writer (`apply_change` find + create), and the round-trip tests' blob builders. The inner `gifs` map stays field 1 of `FavoriteGifs`. All kc-http tests pass.

## GIF picker animation, jump icon, reply-bar corners (2026-11-20)

- Picker tiles (search + trending) didn't animate: Discord's gif results give `src` = an animated `.gif` (media_format=tinygif) but `gif_src` = a provider alternate that Klipy serves as `.webp`, which the GIF animator can't decode. `fill_gif_flow` preferred `gif_src` → no animation. Now it animates from `src`. (Also confirmed live that `gif_search` works with `limit=50`; `limit` under ~20 is what Discord rejects as 50035, not our call.)
- The GIF star is present on every picker tile (`gif_star` in `fill_gif_flow`, both tabs); it is hover-revealed (`kc-gif-star` opacity 0→1) and gold when favourited. "Missing in search" is a stale build / not-hovered, not a code gap.
- Jump-to-latest button icon changed from `go-bottom-symbolic` (arrow + bar, reads as a dot) to `go-down-symbolic` (plain down arrow).
- Reply/edit context bar: the composer below had all four corners rounded, leaving notches where the bar's square bottom met it. `update_composer_corners` adds `kc-attached` to the composer whenever the context bar or upload tray is shown, squaring the composer's top corners so bar + composer read as one rounded box.
- Open/unverified: a specific Klipy `.mp4` gifv in chat not animating — depends on its embed `kind` (only `"gifv"` auto-plays); needs the message's embed data / log to pin down.

## Favorite GIFs are videos: play them in the picker (2026-11-20)

- Favourites loaded (3 stars) but the tiles were blank. Checked the live proto: the saved `src` of each favourite is a VIDEO (`.mp4`/`.webm`) even though `format` is IMAGE (2), and favourites carry no `preview`/`gif_src`. The pixbuf GIF animator can't render a video, and there was no still fallback → blank.
- `fill_gif_flow` now renders by media type: a `.gif` src animates via the pixbuf player; anything else (mp4/webm from favourites) plays as a looping video via `AppState::play_gif` (now `pub(crate)`, the same GStreamer path chat gifv uses), within the animate budget. Trending/search `src` is a `.gif`, so they still use the pixbuf path.
- Unverified offline (needs the account + real media URLs): that the favourite mp4/webm actually plays in a tile; the logic matches the working chat gifv path.
