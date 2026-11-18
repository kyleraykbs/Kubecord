# Structure

High-level module map. Update when a crate is added or its connections change.

```
kylecord (bin) ── kc-ui (GTK4/libadwaita)
                    │
                    ├── kc-state (guilds, channels, messages)
                    │        ▲
                    ├── kc-gateway (user gateway, per account)
                    ├── kc-http (REST v9, rate limits, uploads)
                    └── kc-voice (voice WS, UDP/SRTP, DAVE)
                             └── kc-media (GStreamer)
```

| crate | responsibility |
|---|---|
| `kc-core` | snowflakes, accounts, token store, super-properties, paths |
| `kc-http` | REST v9, headers, rate-limit buckets, uploads, CDN |
| `kc-gateway` | user gateway connection, heartbeat, identify/resume, dispatch |
| `kc-state` | typed Discord models and the event reducer |
| `kc-voice` | voice gateway, UDP, SRTP, RTP, DAVE, stream connections |
| `kc-media` | GStreamer pipelines: mic, playback, camera, screen, codecs |
| `kc-ui` | GTK widgets and the app shell |
| `kylecord` | process entry: GTK main loop + tokio runtime |

Threading: GTK lives on the main thread. Tokio runs on a background runtime; UI
commands go to tokio over channels, tokio events come back through the GLib main
context. No `!Send` GTK type crosses the runtime boundary.

## kc-ui modules

`AppState` (in `shell.rs`) owns every widget and the per-account `Session`s;
each module below adds an `impl AppState` block for one part of the window.

| module | part |
|---|---|
| `shell.rs` | window construction, `AppState`, `spawn` (async work → GTK thread) |
| `app.rs` | startup, login, account switching, event dispatch, voice controls, capture settings |
| `rail.rs` | server rail: home, unread DMs, guilds, folders, pills and badges |
| `sidebar.rs` | channel tree / DM list, navigation (`open_home`, `select_guild`, `open_channel`), acks |
| `chat.rs` | message list: rows, grouping, scroll-back, reactions, message actions, pins, search, typing |
| `content.rs` | message content widgets: text, media previews, embeds, stickers, image viewer |
| `rich.rs` | Discord markdown parser → styled pieces → Pango markup (no GTK) |
| `composer.rs` | sending, replies, edits, uploads, emoji and GIF pickers, autocomplete |
| `friends.rs` | friends page, relationship actions, opening DMs |
| `members.rs` | member list, channel header, user bar, status, profile popover |
| `images.rs` | async image cache (memory + `$XDG_CACHE_HOME/kylecord/images`), WebP decoding, file downloads for playback |
| `player.rs` | inline GIF-style video (`gifv`) playback: muted looping playbin → `gtk4paintablesink`, run only while visible and focused |
| `voice.rs` | voice channel view (participant tiles, speaking ring), double-click-to-join |
| `style.rs` / `widgets.rs` | application CSS and shared widget builders |

`kc-state` keeps the reducer in `state.rs` and every read-only view the UI
needs in `queries.rs` (channel tree, unread, permissions, names, emoji).
