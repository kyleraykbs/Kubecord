# TODO

## Phase 0 — foundation

- [x] Repo, git, agents docs
- [ ] Flake (flake-parts, rust-overlay, crane), devshell, checks
- [ ] Workspace + `kc-core` (snowflake, accounts, token, super-properties)
- [ ] Minimal GTK window, `nix run`

## Phase 1 — protocol

- [ ] `kc-http`: REST v9, headers, rate-limit buckets, uploads
- [ ] `kc-gateway`: identify, heartbeat, resume, dispatch parsing
- [ ] `kc-state`: models + reducer for guilds/channels/messages
- [ ] `kc-probe` CLI: login, list guilds/channels, fetch and send messages

## Phase 2 — chat UI

- [ ] libadwaita shell, account switcher, guild rail, channel list
- [ ] message list/composer, live events, attachments, reactions, replies, typing
- [ ] unread state, presence

## Phase 3 — voice audio

- [ ] voice gateway v9, UDP discovery, SRTP, DAVE
- [ ] receive/decode/mix/playback, mic encode/send, speaking
- [ ] mute/deafen, voice panel

## Phase 4 — video

- [ ] H264 encode/decode, RTP packetization, DAVE video
- [ ] camera send, PipeWire screen capture
- [ ] Go Live create/watch, stream connections, video tiles
- [ ] RTCP NACK/PLI, keyframe requests

## Phase 5 — hardening

- [ ] reconnect/resume, rate-limit behaviour, error surfaces
- [ ] packaging, docs