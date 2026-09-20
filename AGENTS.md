# AGENTS.md

Guidance for AI coding agents working in this repository.

## Project

Cermin is an AirPlay 2 **sender** for Windows and Linux: discovers receivers on the
LAN, pairs via HAP `pair-setup`/`pair-verify`, captures the desktop (DXGI on Windows,
X11 on Linux), encodes H.264 + ALAC audio, and streams over encrypted RTP. Primary
target is older Samsung smart TVs; Apple TV needs the separate `fpsap-helper.exe`.

Rust workspace, edition 2024, resolver 2. Requires Rust 1.95+ (locked `eframe` 0.36.2).
This source archive has no Git metadata — do not rely on `git log`/`git diff`.
`IMPROVEMENTS.md` tracks recent review work and known limitations; keep it updated.

## Commands

Run these before considering a change done (CI runs the same set):

```powershell
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets
cargo build --locked --workspace --all-targets
cargo test --locked --workspace --all-targets
```

Windows GUI build and tests add the `gui` feature:

```powershell
cargo test --locked --workspace --all-targets --features rotten-app/gui
cargo build --locked -p rotten-app --no-default-features --features encode-dll,gui --bins
```

Release build (self-contained folder in `dist\`, downloads OpenH264 if absent):

```powershell
powershell -ExecutionPolicy Bypass -File scripts\build-release.ps1
```

Cross-compile from Linux/WSL: `bash ./scripts/build-windows.sh`.
CLI smoke check without a receiver: `.\target\debug\cermin-cli.exe probe` and `--help`.

Hardware/integration tests are `#[ignore]`d; run explicitly when relevant (the DLL
pipeline test requires the real OpenH264 DLL, the desktop smoke test a real display,
the loopback test a WASAPI endpoint):

```powershell
cargo test --locked -p rotten-video --features software-encode-source,software-encode-dll --test stream_pipeline -- --ignored
cargo test --locked -p rotten-capture --test desktop_smoke -- --ignored
cargo test --locked -p rotten-app --lib real_loopback_starts_and_stops_without_muting -- --ignored
```

## Architecture

```
crates/
  rotten-core/       Config, device/feature bits, ScopedTask, shared session clock
  rotten-discovery/  mDNS browse + manual target resolve
  rotten-crypto/     SRP, ChaCha20-Poly1305, HAP frames, FairPlay SAP
  rotten-pairing/    HAP pair-setup/pair-verify + legacy pair-setup-pin
  rotten-protocol/   RTSP mirror setup, PTP engine, audio RTP (ALAC), timing
  rotten-video/      H.264 encode (OpenH264), frame pacing, encrypted video stream
  rotten-capture/    X11 (Linux) / DXGI (Windows) backends
  rotten-app/        cermin.exe (GUI, `gui` feature) + cermin-cli.exe
  rotten-probe/      cermin-probe.exe, receiver session diagnostic
```

## Conventions and gotchas

- Always use `--locked`; the dependency set is pinned by `Cargo.lock`.
- Encoder features: default is `encode-source` (portable, from source); releases use
  `encode-dll` (official OpenH264 DLL, ~10x faster, needed for smooth 1080p30).
  Do not change feature defaults or let both encoders define the same symbols twice.
- Windows-first: audio capture (WASAPI loopback) is Windows-only; Linux sends silence.
- User-facing runtime knobs are `CERMIN_*` environment variables documented in
  `README.md`. Add new ones there.
- Credentials live at `%APPDATA%\cermin\credentials.json` /
  `~/.config/cermin/credentials.json`; never commit `credentials.json`, API keys, or
  secrets. `.gitignore` already excludes `*.log`, `target/`, `dist/`, `vendor/*.dll`.
- `fpsap-helper` is GPL-3.0 and shipped as a separate executable: never link it into
  the app or copy its code. See `THIRD_PARTY_NOTICES.md` before touching dependencies.
- Long-running work must be owned/cancellable (`ScopedTask`, `JoinSet`) and sessions
  must clean up on failure, Disconnect, and cancellation; follow the patterns in
  `IMPROVEMENTS.md`.
- Hardware was not in the loop for most of the existing tests: receiver interop, A/V
  sync, and mute/loopback behavior are unverified without a real TV. Prefer tests that
  do not require a receiver, and call out what still needs hardware.
