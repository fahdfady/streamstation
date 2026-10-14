I miss the feeling of watching a random movie on the TV the channel just decided to show now. Or when you see a regular mediocre movie that you *just like* and feels good for you. Random cartoons for your children that you don't really press "Play" to attach your kid to sit and watch. I miss the general feeling of not deciding things.

That's why I created this project. It can watch a directory you give it, shuffle, and give you a deterministic schedule just like a TV program and show it one after one. Will keep running even if you're not watching in the browser (the server has to be up)

You can also just launch the server on one machine on the local network and multiple people can watch the streamed media on it with the same schedule. Real-time they watch every buffer together.

## How to Run

```bash
cargo run --release              # default: ~/Videos/Movies, http://127.0.0.1:8080
cargo run --release -- scan      # scan + print the schedule
cargo run --release -- serve --port 9000 --bind 0.0.0.0   # LAN/TV
```

## How it works

Turns your media directory into one endless playlist whose "now playing" is computed as `(wall-clock time − server start) % total library duration`, and it only launches an ffmpeg HLS encoder while at least one viewer's heartbeat is present. Meaning the video is not actually running when you're not there, but the time is being calculated.

- A heartbeat every 5s coming from a Web Worker
- Adds your session id to the viewers list
- If the viewers are >= 1 — launch ffmpeg and keep sending buffers

## License

[MIT](./LICENSE).

Point it at media you own or have the right to watch. This project doesn’t provide or download any content

Additionally, this project is entirely [vibe-coded](https://twitter.com/karpathy/status/1886192184808149383). However, this README is written by me, Fahd.
