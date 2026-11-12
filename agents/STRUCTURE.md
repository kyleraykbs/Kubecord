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