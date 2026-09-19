# RottingApple

PC-to-TV AirPlay screen mirroring with **system audio**, for **Windows** (primary) and **Linux**.

RottingApple acts as an AirPlay 2 **sender**: it discovers receivers on your LAN (Apple TV, Samsung TVs/projectors, ...), pairs with the on-screen code, captures the desktop, encodes H.264, and streams video + audio over AirPlay.

## Quick start (Windows)

1. Build (or download) the release folder — it contains:
   - `rottingapple.exe`
   - `openh264-2.6.0-win64.dll`
   - `fpsap-helper.exe` (only needed for FairPlay receivers, e.g. Apple TV)
2. Double-click **`rottingapple.exe`**.
3. On first run, type the **AirPlay code shown on the TV** when prompted. The pairing is saved, so this happens only once.
4. Your screen and system audio mirror to the TV. The window stays open; press `Ctrl+C` to stop.

No arguments = auto mode: it finds the receiver, pairs if needed, mirrors with audio, and reconnects automatically after Wi-Fi hiccups.

## Features

- mDNS discovery of AirPlay receivers (`_airplay._tcp`)
- HAP `pair-setup`/`pair-verify` pairing (legacy `pair-setup-pin` fallback), credential persistence
- Automatic timing negotiation: **PTP** (Samsung TVs/projectors) or **NTP** (Apple TV)
- No FairPlay required for receivers without FairPlay SAP; `fp-setup` via `fpsap-helper` for Apple TV
- Screen mirroring up to 1080p30, H.264, ChaCha20-Poly1305 encrypted data streams
- **System audio** mirroring (WASAPI loopback on Windows → 44.1 kHz ALAC over RTP, A/V-synced via PTP anchors)
- Windows capture via DXGI Desktop Duplication; Linux capture via X11 (XWayland on Wayland)
- Test mode with a synthetic pattern (no display server needed)
- Optional virtual-display-only capture for extend-like workflows (experimental)

## Requirements

### Windows

- Windows 10/11 on the same network as the TV
- For running: nothing else — the release folder is self-contained
- For building: Rust 1.85+, Visual Studio Build Tools (C++ workload)

### Linux

- Rust 1.85+ (edition 2024), X11 display server
- Audio capture is Windows-only; Linux builds stream video (and silence for audio)

### Receiver

- AirPlay enabled: **Settings → AirPlay**
- Note the 4/6-digit code shown when pairing starts

## Build

### Windows release (recommended)

```powershell
powershell -ExecutionPolicy Bypass -File scripts\build-release.ps1
```

Outputs the ready-to-run folder in `dist\`.

> **Important:** the release build uses the official OpenH264 DLL (`encode-dll` feature) —
> it is roughly **10x faster** than the portable, from-source encoder and is what
> makes smooth 1080p30 possible. The same command manually:
>
> ```powershell
> cargo build --release -p rotten-app --no-default-features --features encode-dll
> ```
>
> The portable build (`cargo build --release`, no DLL required) works everywhere but
> may drop to ~20 fps at 1080p on slower machines.

### Cross-compile from Linux/WSL

```bash
sudo apt install mingw-w64
rustup target add x86_64-pc-windows-gnu
./scripts/build-windows.sh
```

## Usage

```bash
# One-click behaviour: discover, pair if needed, mirror with audio, retry forever
rottingapple

# Discover receivers on the LAN
rottingapple discover

# Pair interactively (enters code when prompted)
rottingapple pair --target 192.168.1.50

# Mirror primary display with system audio
rottingapple mirror --target 192.168.1.50 --audio

# Mirror with options (heavier but sharper)
rottingapple mirror --target 192.168.1.50 --width 1920 --height 1080 --fps 30 --bitrate 30000 --audio

# Test mode (synthetic pattern, no capture)
rottingapple mirror --target 192.168.1.50 --test
```

Run `rottingapple <command> --help` for all options.

### Environment variables

| Variable | Effect |
|---|---|
| `ROTTINGAPPLE_KEEP_LOCAL_AUDIO=1` | Do not mute the local output while mirroring (local audio + TV audio will echo) |
| `ROTTINGAPPLE_AUDIO_LATENCY_MS=<ms>` | Override the A/V playout lead (default 500 ms for non-FairPlay receivers; lower = less lag, may stutter) |
| `ROTTINGAPPLE_DEBUG_LOG=1` | JSON trace to `%TEMP%\rottingapple-debug.log` |
| `ROTTINGAPPLE_AUDIO_TONE=1` | Send a 440 Hz tone instead of captured audio |
| `ROTTINGAPPLE_NO_AUDIO_RTP=1` | Disable the audio RTP stream entirely |
| `ROTTINGAPPLE_NO_AUDIO_SYNC=1` | Disable audio PTP anchor packets (debugging) |

### Credentials

```
Windows: %APPDATA%\rottingapple\credentials.json
Linux:   ~/.config/rottingapple/credentials.json
```

Delete this file to force pairing again on the next run.

### Audio note (echo)

While mirroring, the app mutes your default output device so you hear the TV only.
Some driver stacks (e.g. certain HDMI/Bluetooth devices) silence WASAPI loopback
capture when the endpoint is muted. RottingApple detects that case and restores
local audio automatically (the TV keeps playing sound), so a slight echo may
remain. Set `ROTTINGAPPLE_KEEP_LOCAL_AUDIO=1` to skip muting entirely.

## Architecture

```
crates/
  rotten-core/       Config, device/feature bits, shared session clock
  rotten-discovery/  mDNS browse + manual target resolve
  rotten-crypto/     SRP, ChaCha20-Poly1305, HAP frames, FairPlay SAP
  rotten-pairing/    HAP pair-setup/pair-verify + legacy pair-setup-pin
  rotten-protocol/   RTSP mirror setup, PTP engine, audio RTP (ALAC), timing
  rotten-video/      H.264 encode (OpenH264), frame pacing, encrypted video stream
  rotten-capture/    X11 (Linux) / DXGI (Windows) backends
  rotten-app/        CLI binary (auto mode, wizard, mirror, discover, pair)
```

## Extend display (virtual monitor)

Native AirPlay extend is not available from non-Apple senders. For extend-like behavior:

1. Install a virtual display driver (e.g. [Virtual Display Driver](https://github.com/VirtualDrivers/Virtual-Display-Driver) on Windows).
2. Configure it as an extended desktop in OS display settings.
3. Run `rottingapple mirror --virtual-display --display <index>`.

## Limitations

- Hardware encoding (`--hwaccel nvenc`/`vaapi`) is not implemented yet; OpenH264 software encoding is used.
- FairPlay `fp-setup` needs `fpsap-helper` (GPL-3.0) next to the exe; receivers without FairPlay (bit 14 clear) skip it entirely.
- Corporate networks blocking mDNS require `--target <ip>`.
- Linux audio capture is not implemented (silence is sent when audio is enabled).

## License

MIT OR Apache-2.0 for the RottingApple application code.

Third-party components (OpenH264, Playfair, fpsap-helper) have separate licenses — see [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).
