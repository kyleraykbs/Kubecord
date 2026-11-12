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
- [ ] live verification with a real token

## Phase 3 — voice audio

- [x] voice gateway v9, UDP discovery, SRTP, DAVE
- [x] receive/decode/mix/playback, mic encode/send, speaking
- [x] mute/deafen, voice panel
- [ ] live verification with a real token and a second participant

## Phase 4 — video

- [x] H264 encode/decode, RTP packetization, DAVE video
- [x] camera send through the XDG camera portal
- [x] Go Live create/screen share through the XDG screen-cast portal
- [ ] receive/render camera and Go Live video
- [ ] stream watching (`STREAM_WATCH`, sink wants)
- [ ] RTCP NACK/PLI handling beyond logging keyframe requests
- [ ] live verification

## Phase 5 — hardening

- [ ] reconnect/resume for voice connections
- [ ] rate-limit edge cases, upload progress
- [ ] packaging/docs
- [ ] unread badges per guild, notification integration
- [ ] message search, pins, threads, polls, embeds polish