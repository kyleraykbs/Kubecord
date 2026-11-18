# TODO

## Phase 0 — foundation

- [x] Repo, git, agents docs
- [x] Flake (flake-parts, rust-overlay, crane), devshell, checks
- [x] Workspace + `kc-core` (snowflake, accounts, token, super-properties)
- [x] Minimal GTK window, `nix run`

## Phase 1 — protocol

- [x] `kc-http`: REST v9, headers, rate-limit buckets, uploads
- [x] `kc-gateway`: identify, heartbeat, resume, dispatch parsing
- [x] `kc-state`: models + reducer for guilds/channels/messages
- [x] `kc-probe` CLI: login, list guilds/channels, fetch/send, voice, stream

## Phase 2 — chat UI

- [x] libadwaita shell, account switcher, guild rail, channel list
- [x] message list/composer, live events, attachments, reactions, replies, typing
- [x] unread state, presence
- [x] live gateway verification with Lambda Ignition (channels visible)
- [x] Discord-style layout: voice panel with camera/share-screen/disconnect, user panel, member list, search

## Phase 3 — voice audio

- [x] voice gateway v9, UDP discovery, SRTP, DAVE
- [x] receive/decode/mix/playback, mic encode/send, speaking
- [x] mute/deafen, voice panel
- [x] voice receive/DAVE with a second participant (heard live 2026-11-18); speaking rings from audio
- [ ] voice activity gating for the microphone (transmits continuously today)
- [x] see cameras and watch streams (stream receive verified live 2026-11-18)
- [ ] video polish: fullscreen/pop-out, local camera preview, hardware decode, per-view sink wants

## Phase 4 — video

- [x] H264 encode/decode, RTP packetization, DAVE video
- [x] camera send through the XDG camera portal
- [x] Go Live create/screen share through the XDG screen-cast portal
- [x] Go Live desktop audio via the default sink monitor (SOUNDSHARE)
- [ ] receive/render camera and Go Live video
- [x] camera portal fd lifetime fix
- [x] camera self_video defaults and main-gateway toggle
- [x] Go Live STREAM_SET_PAUSED(false) after create
- [ ] stream watching (`STREAM_WATCH`, sink wants)
- [ ] RTCP NACK/PLI handling beyond logging keyframe requests
- [ ] live verification

## Phase 5 — hardening

- [ ] reconnect/resume for voice connections
- [ ] rate-limit edge cases, upload progress
- [ ] packaging/docs
- [ ] unread badges per guild, notification integration
- [ ] message search, pins, threads, polls, embeds polish

## Phase 6 — Discord/Dissent parity (started 2026-11-18)

Reference clients are in `reference-projects/` (dissent, abaddon, arikawa,
discordo, discord-userdoccers).

- [x] state: deduplicated READY users, DMs, relationships, presences, read states + mentions, mutes, guild folders/order, roles, permissions, emoji, member list ops
- [x] REST: DMs, relationships, profile, status, pins, search, GIFs, bulk ack; gateway presence + member requests
- [x] server rail with icons, folders, unread pills, mention badges, unread DMs
- [x] channel tree (permission-filtered, collapsible categories, voice occupants), DM list, friends page
- [x] message rows: avatars, grouping, replies, system messages, attachments, embeds, stickers, reactions, edit/delete/pin, scroll-back, jump
- [x] composer: reply, edit (Up), uploads + paste image, custom emoji picker, GIF picker, @/#/: autocomplete, typing
- [x] member list (role groups, presence), profile popover, own status + custom status
- [ ] exercise every interactive path with a second account (pickers, uploads, reactions, friend actions were written but only the static views have been checked by screenshot)
- [x] WebP images; inline playback of `gifv` (Tenor/Klipy) GIFs
- [ ] animated GIF attachments and animated custom emoji (first frame only); inline video attachment playback (opens externally)
- [ ] clickable links in messages that also contain custom emoji
- [ ] local echo of sent messages; unread divider; jump-to-present button
- [ ] threads and forum browser; thread member list
- [ ] notifications, AFK presence, quick switcher (Ctrl+K currently focuses search)
- [x] replace the three deprecated `gtk::Dialog`s with libadwaita dialogs
- [ ] member list range subscription on scroll (fixed `[0,199]` today)
- [ ] guild/channel mute and notification settings UI; mark-as-read exists
- [x] GTK layout warnings from negative margins and from resizing during scroll
- [x] voice channel view with participant tiles; double-click a voice channel to join (Join Voice button removed)
- [ ] a few "min height for width" warnings while the Capture Quality dialog is open
