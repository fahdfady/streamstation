# TODO

## Multi-channel (deferred)

v1 ships a single station rooted at one directory (`path` in the config). The goal
later is several directories = several channels (`/movies`, `/shows`, `/cartoons`)
that can be switched in the UI.

What needs to change when we do it:

- **Config**: replace the flat `path`/`extensions` fields with a `[[channels]]`
  array, each entry `{ name, path, extensions?, shuffle_seed? }`.
- **Runtime**: one `StationRuntime` per channel in a `HashMap<String, Arc<StationRuntime>>`
  held by the app state. Each channel keeps its own `Station`, `Presence` set,
  supervisor task, and temp dir (`/tmp/streamstation/<channel>/`).
- **Supervisor**: spawn one `supervise()` task per channel; idle channels stay
  asleep exactly like the single channel does today.
- **Routes**: prefix everything with the channel:
  - `GET  /watch/{channel}` -> player UI for that channel
  - `GET  /api/channels` -> list + now/next per channel
  - `GET  /api/{channel}/status`
  - `POST /api/{channel}/heartbeat`, `POST /api/{channel}/leave`
  - `POST /api/{channel}/rescan`
  - `GET  /hls/{channel}/index.m3u8`, `GET /hls/{channel}/{segment}.ts`
- **HLS temp dirs**: per-channel subdirectory; segment names already include a
  per-run id, so keep that scheme and add the channel dir on top.
- **Scan cache**: one cache file per channel (`~/.cache/streamstation/<channel>.json`).
- **UI**: channel switcher (tabs or a select) that re-points `/hls/...` and
  re-attaches hls.js on change; persist the choice in the URL hash.

## Nice-to-haves

- **Remux instead of transcode** when the file is already browser-safe
  (H.264/AAC in MP4) using `-c:v copy -c:a copy`, falling back to re-encoding
  for AVI/MKV/etc. Needs recording `codec_name`/`pix_fmt` during `scan.rs`
  probing (extend the cache entry).
- **Hardware acceleration**: `h264_nvenc` / `h264_vaapi` / `h264_videotoolbox`
  selected by a config option, with `libx264` as the fallback.
- **Schedule view in the UI**: upcoming titles derived from `position_at()`,
  so you can see what's "on" later tonight.
- **Persistent epoch**: store the station epoch in a small state file so a
  server restart doesn't reset the program grid (currently `epoch = start time`).
- **Admin page**: list channels/files, trigger rescan, show viewer counts.
- **Idle kill polish**: make the grace period configurable per channel and
  expose "stream stops in Ns if nobody tunes in" in the status API.
