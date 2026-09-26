# Cermin

AirPlay screen mirroring **with system audio** for **older Samsung smart TVs** and other AirPlay receivers — plus **experimental Google Cast / Chromecast** support — built for Windows, designed for Southeast Asia.

*Cermin* means "mirror" in Indonesian and Malay — the same word works across the region, and the app is meant to work the same way: no cables, no dongles, just Wi-Fi.

Cermin is a sender for two protocols:

- **AirPlay 2 (primary):** discovers Samsung TVs/projectors, Apple TVs and other
  receivers on your LAN, pairs with the on-screen code, captures the desktop and
  streams H.264 + ALAC system audio over encrypted RTP.
- **Google Cast (experimental):** discovers Google TVs, Chromecasts and Android
  TV devices and streams the desktop **with system audio on Windows** through
  their built-in Default Media Receiver. No TV app or Chrome bridge is required.

Cast uses live HLS with several seconds of buffering — **not** the low-latency
Cast mirroring protocol — and sends unencrypted media on the LAN; see
[Google TV / Chromecast (experimental)](#google-tv--chromecast-experimental).

## Languages

- **Tiếng Việt:** Cermin giúp bạn trình chiếu màn hình và âm thanh từ máy tính Windows lên TV Samsung đời cũ (và các TV hỗ trợ AirPlay khác) qua Wi-Fi, không cần cáp. Hỗ trợ thử nghiệm Google Cast/Chromecast.
- **ไทย:** Cermin ช่วยฉายหน้าจอและเสียงจากคอมพิวเตอร์ Windows ไปยังสมาร์ททีวี Samsung รุ่นเก่า (และทีวีที่รองรับ AirPlay) ผ่าน Wi-Fi โดยไม่ต้องใช้สาย รองรับ Google Cast/Chromecast (ทดลอง)
- **Bahasa Indonesia:** Cermin memproyeksikan layar dan suara dari PC Windows ke smart TV Samsung lama (dan TV lain yang mendukung AirPlay) lewat Wi-Fi, tanpa kabel. Dukungan eksperimental Google Cast/Chromecast.
- **Bahasa Melayu:** Cermin memaparkan skrin dan audio daripada PC Windows ke TV Samsung lama (dan TV lain yang menyokong AirPlay) melalui Wi-Fi, tanpa wayar. Sokongan eksperimen Google Cast/Chromecast.
- **Filipino:** Ipinapadala ng Cermin ang screen at audio mula sa Windows PC papunta sa lumang Samsung smart TV (at iba pang AirPlay TV) sa pamamagitan ng Wi-Fi, walang cable. Experimental na suporta sa Google Cast/Chromecast.

## Quick start (Windows)

1. Download `Cermin-<version>-windows-x64.zip` from the
   [latest release](https://github.com/Aldathor/Cermin/releases/latest) and extract it
   (or build it yourself with `scripts\build-release.ps1`) — it contains:
   - `cermin.exe` (GUI) and `cermin-cli.exe` (command line)
   - `openh264-2.6.0-win64.dll`
   - `fpsap-helper.exe` (only needed for FairPlay receivers, e.g. Apple TV)
2. Double-click **`cermin.exe`**.
3. Press **Search**, pick your TV from the list and press **Connect**.
4. On first run, type the **AirPlay code shown on the TV** and press Enter. The pairing is saved, so this happens only once.
5. Your screen and system audio mirror to the TV. **Disconnect** stops the session; the volume slider sets the TV's volume.

Prefer the terminal? `cermin-cli.exe` with no arguments does the same thing from a command prompt and retries automatically after Wi-Fi hiccups.

**Google Cast / Chromecast receivers appear in the same list.** They are labeled
**Google Cast**, skip the AirPlay code and hide the AirPlay volume slider. Cast
adds two selectors before connecting — **Cast quality** (Balanced/High) and
**Cast latency** (Stable/Lower delay) — and system audio is on by default on
Windows. Read the trade-offs and current limitations in
[Google TV / Chromecast (experimental)](#google-tv--chromecast-experimental)
before the first Cast session.

## Features

- Windows GUI: search for TVs, one-click connect/disconnect, first-run code prompt and a TV volume slider
- mDNS discovery of AirPlay receivers (`_airplay._tcp`)
- Experimental Google Cast discovery (`_googlecast._tcp`) with manual IP entry and connection diagnostics
- Cast quality presets — **Balanced** (up to 720p/4 Mbps) and **High** (true visible 1080p/8 Mbps) — plus **Stable** and **Lower delay** HLS buffering modes
- Cast system audio on Windows: AAC-LC 44.1 kHz stereo 128 kbps captured from WASAPI playback loopback (not the microphone), with jitter-tolerant packet continuity
- HAP `pair-setup`/`pair-verify` pairing (legacy `pair-setup-pin` fallback), credential persistence
- Automatic timing negotiation: **PTP** (Samsung TVs/projectors) or **NTP** (Apple TV)
- No FairPlay required for receivers without FairPlay SAP; `fp-setup` via `fpsap-helper` for Apple TV
- AirPlay screen mirroring up to 1080p30, H.264, ChaCha20-Poly1305 encrypted data streams
- **System audio** mirroring (WASAPI loopback on Windows → 44.1 kHz ALAC over RTP, A/V-synced via PTP anchors)
- Windows capture via DXGI Desktop Duplication; Linux capture via X11 (XWayland on Wayland)
- Test mode with a synthetic pattern (no display server needed)
- Optional virtual-display-only capture for extend-like workflows (experimental)

## Requirements

### Windows

- Windows 10/11 (x64) on the same network as the TV
- For running: nothing else — the release folder is self-contained
- For building: Rust 1.95+, Visual Studio Build Tools (C++ workload and Windows SDK)
- The release script downloads OpenH264 over HTTPS when it is absent; install Python or 7-Zip to extract it, or place `openh264-2.6.0-win64.dll` in `vendor\` first
- Go 1.21+ is needed to build `fpsap-helper.exe` for FairPlay receivers

### Linux

- Rust 1.95+, a C/C++ compiler, and an X11 display server
- Audio capture is Windows-only; Linux builds stream video (and silence for audio)

### Receiver

For AirPlay:
- AirPlay enabled: **Settings → AirPlay**
- Note the 4/6-digit code shown when pairing starts

For Google Cast: a video-capable Google Cast receiver on the same LAN. Windows
11's **Win+K uses Miracast**, not Google Cast: absence from that list is not an
indication that Cermin cannot discover the TV. The TV may need Internet access
to load Google's Default Media Receiver.

## Build

### Windows release (recommended)

```powershell
powershell -ExecutionPolicy Bypass -File scripts\build-release.ps1
```

Outputs the x64 release folder in `dist\`. Add the target with
`rustup target add x86_64-pc-windows-msvc` if your Rust installation uses a different host target.
The Rust 1.95 requirement comes from the locked GUI dependency (`eframe` 0.36.2).

> **Important:** the release build uses the official OpenH264 DLL (`encode-dll` feature) —
> it is roughly **10x faster** than the portable, from-source encoder and is what
> makes smooth 1080p30 possible. The same command manually:
>
> ```powershell
> cargo build --locked --release -p rotten-app --no-default-features --features encode-dll,gui --bins
> ```
>
> (drop `,gui` if you only want the `cermin-cli.exe` command line tool)
>
> The portable build (`cargo build --release`, no DLL required) works everywhere but
> may drop to ~20 fps at 1080p on slower machines.

### Cross-compile from Linux/WSL

```bash
sudo apt install mingw-w64 curl bzip2
rustup target add x86_64-pc-windows-gnu
bash ./scripts/build-windows.sh
```

Install Go 1.21+ to include the FairPlay helper. Output is in
`target/x86_64-pc-windows-gnu/release/` (or under `CARGO_TARGET_DIR` if set).
Copy both application executables and the DLLs/helper listed by the script together.

For a local CLI startup check that does not contact a receiver:

```powershell
.\dist\cermin-cli.exe probe
.\dist\cermin-cli.exe --help
```

`cermin-probe.exe` is a separate receiver session diagnostic and needs a reachable TV.

## Usage

```bash
# One-click behaviour from the terminal: discover, pair if needed, mirror with audio, retry forever
cermin-cli

# Discover receivers on the LAN
cermin-cli discover

# List monitors and their indices
cermin-cli displays

# Pair interactively (enters code when prompted)
cermin-cli pair --target 192.168.1.50

# Mirror primary display with system audio
cermin-cli mirror --target 192.168.1.50 --audio

# Mirror a specific monitor
cermin-cli mirror --target 192.168.1.50 --display 1 --audio

# Mirror with options (width/height scale the capture down; 0 = match the display)
cermin-cli mirror --target 192.168.1.50 --width 1280 --height 720 --fps 30 --bitrate 30000 --audio

# Test mode (synthetic pattern, no capture)
cermin-cli mirror --target 192.168.1.50 --test
```

Run `cermin-cli <command> --help` for all options.

### Google TV / Chromecast (experimental)

Cast support is included in **v0.1.3 and later** — download the
[latest release](https://github.com/Aldathor/Cermin/releases/latest). For Windows
playback performance, prefer `scripts\build-release.ps1`: it builds the optimized
DLL encoder and packages the matching OpenH264 DLL in `dist/`. A portable
source-encoder GUI build is also available, but can have substantially less
encoding headroom:

```powershell
cargo build --locked --release -p rotten-app --features gui --bins
.\target\release\cermin.exe
```

In the GUI, press **Search**, select a receiver labeled **Google Cast**, choose
the screen and press **Connect**. If multicast discovery is blocked, enter the
TV's current IP under **Google Cast IP / hostname**, press **Add**, then
**Connect**. This uses port 8009; the CLI accepts a custom port. Cast does not
use an AirPlay PIN or saved AirPlay credentials. Its PIN/volume controls are
hidden. **Disconnect** requests receiver STOP and releases the local stream.
On Windows, **System audio (AAC)** is enabled by default; turn it off for a
video-only cast. The audio source is the default Windows playback device, not
the microphone. Local speaker volume/mute is left unchanged.

Two independent Cast-only selectors are available before connecting:

| Setting | Choice | Behavior |
|---|---|---|
| **Cast quality** | **Balanced** (default) | Up to 1280×720, 4 Mbps video, target 30 fps |
| | **High** | Up to true visible 1920×1080, 8 Mbps video, target 30 fps; more CPU/network capacity needed |
| **Cast latency** | **Stable** (default) | About one-second segments, two-second HLS target, eight-second initial media buffer |
| | **Lower delay (experimental)** | About half-second segments, one-second HLS target, four-second initial media buffer; CLI name `responsive` |

Choices are fixed for a session: **Disconnect** before changing them. High
preserves a native Full-HD desktop without downscaling to 720p; smaller sources
are not upscaled. The encoder signals its internal padding through normal H.264
cropping, so 1920×1080 decodes as 1080 visible lines, not 1088 or 1072.

Connection prepares the selected initial buffer, then launches the receiver.
The TV adds its own startup buffering, so these are **not end-to-end delay
guarantees**. In one Skyworth comparison, High/Responsive reached PLAYING in
about 11 seconds versus 23 seconds for High/Stable, but a sustained reduction
in capture-to-panel delay was not directly measured. Use Stable if the lower
buffering margin causes pauses. This is buffered HLS, not sub-second mirroring.
In both modes the playlist offers about 12 seconds of recent media; segments
that leave it stay downloadable for their required retry period. That extra
history improves recovery without instructing the TV to play from the oldest
segment. It does not eliminate HLS latency.

For a cautious first test, use the CLI (replace the example IP with the current
address from discovery or the TV's network settings):

```powershell
# List only Google Cast video receivers; normal "discover" lists both protocols
.\target\release\cermin-cli.exe discover --protocol cast --timeout 5

# Connect and read status only: no app launch, capture or streaming
.\target\release\cermin-cli.exe cast-probe --target 192.168.1.50

# Synthetic pattern + quiet tone pulses on Windows (no desktop/audio capture)
# Stop cooperatively after 30s; a white marker follows the tone's clock phase
.\target\release\cermin-cli.exe cast --target 192.168.1.50 --test --duration 30

# Desktop + system audio on Windows; Ctrl+C stops and cleans up
.\target\release\cermin-cli.exe cast --target 192.168.1.50 --display 0

# Sharper Full-HD video, retaining the established buffering policy
.\target\release\cermin-cli.exe cast --target 192.168.1.50 --quality high

# Optional shorter-segment / lower-startup-buffer trial, still with system audio
.\target\release\cermin-cli.exe cast --target 192.168.1.50 --quality high --latency responsive

# Explicit video-only fallback (also silences the synthetic tone with --test)
.\target\release\cermin-cli.exe cast --target 192.168.1.50 --no-audio
```

`cast` without `--target` selects a receiver only if discovery finds exactly one.
`--duration` includes setup time; cleanup may take a few additional seconds.
Existing `mirror`, `pair` and no-subcommand auto mode remain **AirPlay-only**.
No automatic takeover of another active Cast media app is attempted: stop that
cast first. A `PLAYING` message means the receiver reports playback, not proof
that the picture is visible on the panel.

**Limits and network/privacy requirements:**

- Windows audio uses **WASAPI loopback and the built-in Media Foundation AAC
  encoder**; no FFmpeg or additional audio codec DLL is shipped. Windows N or
  codec-stripped installations may need the Media Feature Pack; use `--no-audio`
  if AAC or the playback endpoint is unavailable. Linux remains video-only.
  If the Windows media libraries themselves are missing, install the Media
  Feature Pack even to launch this Windows build; that configuration is untested.
- Audio and video share a per-session clock. Timestamped PCM is resampled to
  44.1 kHz; silence fills idle/gap intervals without compressing the timeline.
  A bounded interleaver waits for both tracks before publishing a segment.
  This prevents known sources of drift but does not guarantee perfect lip-sync
  on every receiver/driver. Endpoint loss fails the session; reconnect after
  changing the default playback device. Protected playback may not be capturable.
  Continuous captured packets are joined by their actual converted sample counts
  so timestamp jitter does not create crackling gaps. Adaptive clock-rate
  correction is not implemented: a device/session clock mismatch exceeding
  100 ms stops the session with a reconnect diagnostic instead of drifting forever.
- Cermin **does not mute local audio or set Cast TV volume**. Expect delayed
  echo if both speakers are audible; use headphones or an appropriate output
  setup rather than assuming muting the playback endpoint preserves loopback.
  The TV remote controls receiver volume.
- Desktop video is fitted inside the selected 720p/4 Mbps or 1080p/8 Mbps envelope,
  targeting 30 fps. Actual frame rate depends on CPU/encoder; a source/debug build
  can be much slower. High uses more bandwidth, and shorter segments require more
  frequent keyframes and HTTP requests. HLS
  buffering is unsuitable for gaming or latency-sensitive interaction.
- Use a **trusted private LAN**. Cast control uses TLS with handshake-signature
  verification but **does not authenticate the receiver certificate identity**.
  Media is **unencrypted HTTP** on the laptop's selected LAN interface, restricted
  to the receiver IP and a random per-session URL. These restrictions are not
  encryption and do not protect against a hostile LAN participant. Do not forward
  the port or expose it to the Internet.
- Allow Cermin through Windows Firewall on the trusted **Private** network; do
  not disable the firewall. The TV must fetch the stream from the laptop. For a
  specific permitted inbound TCP port use `cast --http-port 9123`. Discovery
  needs mDNS (UDP 5353); control normally uses TCP 8009. VPN routing, guest Wi-Fi
  and client isolation can block either direction even on the same SSID.
- The Cast media path buffers segments in bounded memory, not video files.
  Capture resolution changes require reconnecting. Severe capture stalls fail
  the session rather than advertising invalid HLS segments. A blocking OS
  capture call can outlive the bounded shutdown wait until it returns.
- If the receiver replaces the media session, Cermin relinquishes it. If media
  ownership cannot be verified during shutdown, Cermin leaves the app running
  rather than risk stopping someone else's playback; use the TV's controls.
- Native low-latency Cast Streaming, Cast device-auth verification, Linux audio, and
  long-duration/cross-receiver compatibility are follow-up work.

CPU-only encode → HLS → HTTP → independent per-segment decode regression:

```powershell
cargo test --locked -p rotten-video --features software-encode-source --test cast_pipeline
```

Windows CPU-only AAC encode/decode and combined A/V transport regressions
(generate PCM in memory; do not capture or play audio):

```powershell
cargo test --locked -p rotten-app --lib cast_audio
```

#### Choppy video versus playback delay

HLS adds several seconds of **delay**, but it should still display continuous
motion after startup. A new still image every few seconds is not the intended
frame rate. To separate capture/encoding performance from receiver playback:

```powershell
# Local capture, scaling, H.264 encoding and muxing; no TV/audio capture or files
.\target\release\cermin-cli.exe cast-benchmark --display 0 --duration 10

# Same synthetic workload as Cast test mode, without using the desktop
.\target\release\cermin-cli.exe cast-benchmark --test --duration 10

# Measure the actual higher-quality/lower-delay configuration locally
.\target\release\cermin-cli.exe cast-benchmark --display 0 --quality high --latency responsive --duration 10
```

The summary identifies the backend (DXGI/GDI), resolution, source/DLL encoder,
debug/release build, quality/latency presets, bitrate, captured/encoded FPS,
stage timings and segment count. Synthetic benchmarks use 1280×720 for Balanced
and 1920×1080 for High, matching the live test workload.
Sampled pixel-change counts only describe changes at a small grid of points;
a stationary desktop should have few changes, and small movements can be missed.
No pixels or sample hashes are printed or saved. Benchmark audio is disabled,
so it does not prove the whole A/V session or TV presentation is healthy.

During a CLI `cast` session, periodic pipeline and HTTP-delivery summaries show
whether frames/segments are produced continuously and whether the receiver is
missing segment requests or writes fail. Media-state/time polling distinguishes
normal startup buffering from repeated stalls. Receiver `PLAYING` and sender FPS
still do not prove that the TV displays every frame. When available,
`live_edge_lag_seconds` is the receiver's distance from its **reported seekable
end**, not capture-to-panel delay; that endpoint can itself depend on the HLS
profile. Do not interpret it alone as a latency comparison. Keep the optimized encoder's
`openh264-2.6.0-win64.dll` beside its executable; do not mix it up with the portable
source build or an older `dist/` executable.

Delivery diagnostics also show advertised window duration, retained payload
bytes/segment count and playlist age. Retention is timed from removal from the
advertised playlist, not merely from original production. The store is capped
at 128 MiB of segment payload and 64 entries; if it cannot respect both retention
promises and those caps, it fails explicitly rather than silently deleting
segments a receiver may still need. The stream is not a DVR: an arbitrarily
long pause or network outage can still require reconnection.

### Multiple monitors

`cermin-cli displays` lists every capturable monitor with the index accepted by
`--display`:

```text
Found 2 display(s):

  #0 \\.\DISPLAY1 — 1920x1080
  #1 \\.\DISPLAY2 — 2560x1440 (virtual)

Pick one with: cermin-cli mirror --display <index>
```

The GUI (`cermin.exe`) shows the same list in a "Screen to mirror" picker above
the Connect button. Without a selection, the primary monitor (index 0) is
captured.

### Environment variables

| Variable | Effect |
|---|---|
| `CERMIN_KEEP_LOCAL_AUDIO=1` | Do not mute the local output while mirroring (local audio + TV audio will echo) |
| `CERMIN_TV_VOLUME=<percent>` | TV volume set at session start (default 35; 0 = quietest, 100 = max) |
| `CERMIN_AUDIO_LATENCY_MS=<ms>` | Override the A/V playout lead (default 500 ms for non-FairPlay receivers; lower = less lag, may stutter) |
| `CERMIN_DEBUG_LOG=1` | JSON trace to `%TEMP%\cermin-debug.log` |
| `CERMIN_AUDIO_TONE=1` | Send a 440 Hz tone instead of captured audio |
| `CERMIN_NO_AUDIO_RTP=1` | Disable the audio RTP stream entirely |
| `CERMIN_NO_AUDIO_SYNC=1` | Disable audio PTP anchor packets (debugging) |
| `CERMIN_ENCODER_THREADS=<n>` | OpenH264 thread count (default: available cores, capped at 4) |
| `CERMIN_ENCODER_RC=<mode>` | Encoder rate control: `bitrate` (default), `buffer`, or `quality` |
| `CERMIN_CAPTURE_BACKEND=<name>` | Windows capture backend: `auto` (default), `dxgi`, or `gdi` |

### Windows capture and hybrid GPUs

Windows capture uses DXGI Desktop Duplication. Microsoft does not support
duplication against the **discrete GPU** of a hybrid laptop (Intel + NVIDIA/AMD):
the call fails with `DXGI_ERROR_UNSUPPORTED`. Cermin handles this automatically:

1. It falls back to a slower **GDI** capture backend so the session still works.
2. It sets the per-app Windows preference (`GpuPreference=1`) for
   `cermin.exe`/`cermin-cli.exe` under
   `HKCU\Software\Microsoft\DirectX\UserGpuPreferences`, asking Windows to run
   Cermin on the integrated GPU. **Restart Cermin** to get the faster DXGI path.
   The preference is keyed to the executable path, so moving the folder requires
   setting it again.

The same fallback covers Remote Desktop sessions and some virtual machines.
Force a backend for testing with `CERMIN_CAPTURE_BACKEND=dxgi` or `=gdi`.
`cermin-cli capture-probe` prints the active backend and grabs three frames, and
`cermin-cli displays` lists the graphics adapter behind each monitor.

### Credentials

```
Windows: %APPDATA%\cermin\credentials.json
Linux:   ~/.config/cermin/credentials.json
```

Delete this file to force pairing again on the next run.

### Audio note (echo)

While mirroring, the app mutes your default output device so you hear the TV only.
Some driver stacks (e.g. certain HDMI/Bluetooth devices) silence WASAPI loopback
capture when the endpoint is muted. Cermin detects that case and restores
local audio automatically (the TV keeps playing sound), so a slight echo may
remain. Set `CERMIN_KEEP_LOCAL_AUDIO=1` to skip muting entirely.

## Architecture

```
crates/
  rotten-core/       Config, device/feature bits, shared session clock
  rotten-discovery/  mDNS browse + manual target resolve
  rotten-crypto/     SRP, ChaCha20-Poly1305, HAP frames, FairPlay SAP
  rotten-pairing/    HAP pair-setup/pair-verify + legacy pair-setup-pin
  rotten-protocol/   RTSP mirror setup, PTP engine, audio RTP (ALAC), timing
  rotten-video/      H.264 encode (OpenH264), frame pacing, encrypted video stream
  rotten-capture/    X11 (Linux) / DXGI + GDI fallback (Windows) backends
  rotten-cast/       Google Cast control (TLS), HLS mux/store, token-gated local HTTP media server
  rotten-app/        cermin.exe (GUI) + cermin-cli.exe (command line)
  rotten-probe/      cermin-probe.exe, receiver session diagnostic
```

## Extend display (virtual monitor)

Native AirPlay extend is not available from non-Apple senders. For extend-like behavior:

1. Install a virtual display driver (e.g. [Virtual Display Driver](https://github.com/VirtualDrivers/Virtual-Display-Driver) on Windows).
2. Configure it as an extended desktop in OS display settings.
3. Run `cermin-cli mirror --virtual-display --display <index>`.

## Limitations

- Hardware encoding (`--hwaccel nvenc`/`vaapi`) is not implemented yet; OpenH264 software encoding is used.
- FairPlay `fp-setup` needs `fpsap-helper` (GPL-3.0) next to the exe; receivers without FairPlay (bit 14 clear) skip it entirely.
- Corporate networks blocking mDNS require `--target <ip>`.
- Linux audio capture is not implemented (silence is sent when audio is enabled).
- Google Cast support is experimental: buffered live HLS (seconds of delay), unauthenticated receiver certificate identity, unencrypted LAN media, and no Linux Cast audio or native low-latency Cast Streaming.

## Support

If Cermin is useful to you, you can buy me a coffee:

<a href="https://buymeacoffee.com/pabloarancg"><img src="https://img.shields.io/badge/Buy%20Me%20a%20Coffee-support-yellow?logo=buymeacoffee&logoColor=white" alt="Buy Me a Coffee"></a>

## License

MIT OR Apache-2.0 for the Cermin application code.

Third-party components (OpenH264, Playfair, fpsap-helper) have separate licenses — see [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).
