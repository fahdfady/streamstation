# TODO

## Subtitle support

Show a subtitle picker on `/` only when the movie actually has sidecar subtitle
files next to it.

- **Detection**: during `scan.rs` probing, look for files sharing the movie's
  stem in the same directory with subtitle extensions (`.srt .ass .ssa .vtt
  .sub`), including language suffixes (`Movie.en.srt`, `Movie.ara.srt`). Store
  the list in the duration cache entry and expose it via `/api/status`.
- **Delivery (primary)**: mux a WebVTT track into the HLS stream by adding
  `-c:s webvtt` (and mapping only the selected track) to the ffmpeg command, so
  hls.js exposes native subtitle track selection. The chosen track lives in the
  station state so all viewers get it (or make it per-client later).
- **Delivery (fallback)**: serve converted WebVTT from the server
  (`GET /subtitles/{index}.vtt`, convert SRT -> VTT on the fly or via ffmpeg)
  and attach with a `<video><track>` element client-side.
- Burn-in via `-vf subtitles=...` is an option if track muxing proves flaky,
  but then subtitles can't be toggled per viewer.

## File health ping in /admin

Right now reachability comes from a cheap existence + duration-cache check
during reconciliation. Add an explicit deep check:

- When the admin page opens (or via a "Verify all" button), `ffprobe` every
  scheduled file on demand and show a **corrupted** badge beside bad ones -
  distinct from "missing" - with the ffprobe error in the tooltip.
- Cache results per file (mtime+size) so repeat visits don't re-probe the whole
  library; run probes with bounded concurrency like `scan.rs` does.
- Consider a per-row "ping" button for single-file re-checks.

## Multi-channel (deferred)

v1 ships a single station rooted at one directory (`path` in the config). The goal
later is several directories = several channels (`/movies`, `/shows`, `/cartoons`)
that can be switched in the UI.

What needs to change when we do it:

- **Config**: replace the flat `path`/`extensions` fields with a `[[channels]]`
  array, each entry `{ name, path, extensions?, shuffle_seed? }`.
- **Runtime**: one `StationRuntime` per channel in a `HashMap<String, Arc<StationRuntime>>`
  held by the app state. Each channel keeps its own `Station`, `Presence` set,
  supervisor task, temp dir (`/tmp/streamstation/<channel>/`) and
  `schedule.toml`.
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
- **Admin**: channel switcher on `/admin`; one schedule file per channel.

## Nice-to-haves

- **Remux instead of transcode** when the file is already browser-safe
  (H.264/AAC in MP4) using `-c:v copy -c:a copy`, falling back to re-encoding
  for AVI/MKV/etc. Needs recording `codec_name`/`pix_fmt` during `scan.rs`
  probing (extend the cache entry).
- **Hardware acceleration**: `h264_nvenc` / `h264_vaapi` / `h264_videotoolbox`
  selected by a config option, with `libx264` as the fallback. (This machine
  has an RTX 3050 + Intel iGPU, both verified working with ffmpeg.)
- **Schedule view in the UI**: upcoming titles derived from `position_at()`,
  so you can see what's "on" later tonight.
- **Idle kill polish**: make the grace period configurable per channel and
  expose "stream stops in Ns if nobody tunes in" in the status API.
- **LAN / offline playback**: multi-viewer over LAN already works via
  `--bind 0.0.0.0`; the player still loads hls.js from a CDN, so TV/phone
  clients without internet can't initialize. Vendoring hls.js locally would
  make the station fully offline.
