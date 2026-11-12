# Terminology

| user term | maps to |
|---|---|
| gateway | `kc-gateway`, the user WebSocket at `wss://gateway.discord.gg/?v=9&encoding=json` |
| voice gateway | `kc-voice` WebSocket, `wss://{endpoint}/?v=9` |
| stream / go live | main-gateway op 18/19/20/22 plus a second voice connection |
| account | one row in `AccountStore`, one gateway task, at most one voice connection |