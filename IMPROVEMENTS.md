# Mirroring program review and improvements

Reviewed on 2026-09-19. This source archive has no Git metadata; this document
tracks the plan, implemented changes, evidence, and remaining work.

## Plan and outcome

- [x] Review capture, encoding, transport, session ownership, audio, pairing storage and builds.
- [x] Own background session tasks and clean up on failure, Disconnect and cancellation.
- [x] Fix frame-channel closure and test replacement, cancellation and final-frame delivery.
- [x] Align non-Windows audio startup and verify Windows/Linux compilation paths.
- [x] Harden RTSP response parsing and test malformed, truncated and encrypted responses.
- [x] Replace credentials safely without truncating the previous file first.
- [x] Verify actual encoding/decoding, desktop capture, audio startup, builds and formatting.
- [x] Complete five Astra/high reviews and two Astra/Ultra final audits; record limitations.

## Implemented changes

### Cast captured-audio packet continuity (2026-09-26)

The user confirmed that video works well in both quality and latency modes,
including Responsive, but reported static/distortion in system audio in all
modes. Found and reproduced a shared capture-timeline defect, not a video-preset
defect: each packet's QPC timestamp was independently floored to an output frame
and its resampler phase independently rounded. Even contiguous input could
therefore acquire one-frame overlaps or holes, which downstream trimmed or
filled with silence. A fractional-anchor, variable-packet regression reproduced
77 broken boundaries out of 255 at both 48 and 96 kHz without timestamp jitter;
simulated alternating +/-100 us jitter broke every boundary at all three tested
rates (44.1/48/96 kHz). These are synthetic reproductions, not measurements of
the user's endpoint jitter.

- Cast capture now anchors each continuous device run once and advances by the
  number of converted frames actually produced. QPC remains the initial anchor
  and clock watchdog, not a per-packet sample insertion/deletion instruction.
- Real WASAPI discontinuities/device-position gaps reset both resampler and
  timeline. Downstream queue drops still leave true holes; startup timestamps
  remain signed, and zero-output packets do not consume a carried position.
  The existing AirPlay resampler/capture path, AAC configuration, video and HLS
  presets are unchanged. No mute/volume changes or new dependencies.
- Added exact-continuity and waveform-stitch regressions, gap/drop/zero-output
  coverage, checked arithmetic and clock-watchdog boundaries. Both original
  reproductions now report zero broken boundaries at all three rates.
- Added a real MF AAC encode/decode stereo-chirp fidelity test with one bounded
  global alignment, per-channel and per-block checks. Measured correlation
  about 0.99996 and SNR about 41 dB; negative controls reject channel swaps,
  duplicated channels, zeroed/dropped/repeated blocks and injected static.

Verification: required formatting, locked workspace Clippy/build, default/GUI
tests and DLL/GUI build passed (existing warnings remain): **349 default tests
passed / 362 GUI-enabled tests passed**, four hardware tests ignored in each.
Optimized DLL release rebuilt in `target/release`; CLI `probe` and GUI window
startup/graceful exit passed. `dist/` was not repackaged. No TV session, audio
playback or real capture was run for this fix; audible improvement on the TV
still needs user confirmation. The local tests establish the defect and fix,
not that it is the only possible source of distortion.

Limitation: this is not adaptive sample-rate correction. A continuous device
clock differing from QPC by more than 100 ms ends the session with an audio
clock mismatch/reconnect diagnostic rather than accumulating unlimited A/V
drift or repeatedly inserting/deleting samples. Long-session hardware clock
behavior and receiver lip-sync remain unverified.

Published as **v0.1.3** (`Cermin-v0.1.3-windows-x64.zip`, tag `v0.1.3`): the
workspace version was bumped, `dist/` repackaged from the locked release build,
and the README, repository description and topics updated to lead with the new
Cast support.

### Cast quality and optional shorter-segment mode (2026-09-26)

- Added independent Cast quality and latency selectors to GUI, `cast` and
  `cast-benchmark`. Defaults stay Balanced/Stable (720p, 4 Mbps, existing buffer
  policy). High fits inside true 1920x1080 at 8 Mbps; Responsive uses roughly
  500 ms segments, a one-second target and four-second readiness. Both keep the
  twelve-second advertised history, timed retired-segment retention, 128 MiB
  payload cap and 64-entry cap. No forced live seek or new dependencies.
- High uses an opt-in exact-visible encoder constructor. Real even input
  dimensions reach OpenH264, which macroblock-pads and signals SPS cropping.
  Tests decode 1920x1080 and non-16-aligned dimensions correctly, including the
  right/bottom pixels. Existing AirPlay fitting/padding and encoder feature
  defaults are unchanged. Invalid exact dimensions/rates fail before encoding.
- Muxer/store timing, forced-IDR cadence and UI startup copy share the selected
  profile. Responsive snapshot cadence is 500 ms, not an integer-division zero.
  Master-playlist bandwidth hints are 8/16 Mbps for Balanced/High respectively,
  allowing headroom over video plus AAC. Settings are snapshotted on Connect;
  changing quality requires reconnecting rather than changing SPS midstream.
- Added responsive half-second encode/mux/decode motion coverage with exact
  cropping, per-segment independent decode and PTS/continuity checks. Retention
  is exercised over twenty simulated minutes of shorter jittered segments.
- Added numeric receiver seekable-end distance to diagnostics. It deliberately
  does not compare the sender's clock with the receiver's timeline or claim to
  measure capture-to-panel latency.

Measured on the local laptop: High/Responsive desktop-only benchmark encoded
289 Full-HD frames in 10.01 s (~28.9 fps), using the optimized DLL and no scaling.
The controlled TV tests used generated Full-HD video and tone, not captured
desktop/audio. High/Stable (90 s) produced 2,688 frames and 87 segments;
High/Responsive (300 s) produced 8,995 frames and 576 segments, approximately
30 fps. Both completed with exit 0 and the TV idle afterward. Responsive served
562 segment responses with no missing segments or failed writes, retained about
21 MB, and had one roughly one-second rebuffer just after initial PLAYING with
no later BUFFERING report. The first PLAYING report was about 23.3 s after
starting Stable and 11.2 s after starting Responsive (12.8 s after its early
rebuffer recovery).

This demonstrates working Full-HD delivery and faster startup in these runs,
not a proven reduction in steady glass-to-glass delay. The receiver's reported
distance to its seekable end settled near 2.5 s for Stable and 3.94 s for
Responsive; the seekable endpoint itself is profile-dependent, so those values
are not comparable end-to-end latency measurements. Visual image quality,
real-content motion and perceived delay still require user observation.

Final verification on Windows: formatting, locked workspace Clippy/build,
default and GUI-enabled workspace tests, and the DLL/GUI debug build all passed
(existing warnings remain). Default suite: **336 passed, 4 ignored**; GUI suite:
**349 passed, 4 ignored**. The four skipped tests require desktop/audio hardware.
Built `exact_geometry`, `exact_responsive`, `cast_pipeline` and `stream_pipeline`
with both `software-encode-source,software-encode-dll`, then ran their compiled
executables with `--include-ignored` from `dist/`, where the official DLL is
present: **13 passed, none ignored**. This configuration selects the DLL encoder
and source decoder, covering true 1080p cropping, legacy 1088-line behavior,
half-second segment motion, HLS/HTTP delivery and the existing TCP pipeline.

The optimized DLL/GUI release build is current in `target/release`, with its
OpenH264 DLL alongside. Release CLI help, Cast preset help and `probe` passed;
the GUI opened a window and closed gracefully with exit 0. No Cermin processes
remained afterward. Recorded verification only in this final pass: no new TV
session or real desktop/audio capture was started, and `dist/` was not
repackaged. Linux and broader receiver compatibility remain unverified here.

### Cast live-window retention and recurring buffering (2026-09-26)

The user reported alternating smooth video and a five-second slideshow after
several minutes, with a loading spinner visible on the TV. The earlier producer
FPS and 30-second delivery test did not establish stable receiver presentation.

- Found a concrete RFC 8216 section 6.2.2 defect: the six-entry live playlist
  retained only two additional entries, so a typical removed one-second segment
  disappeared after about two seconds rather than its duration plus the longest
  advertised playlist containing it. Counting six entries also failed to enforce
  a six-second minimum window for allowed 0.9-second segments.
- HLS readiness now requires eight advertised media seconds. The steady window
  is a newest contiguous suffix of at least 12 seconds, instead of six entries.
  Target duration remains two seconds; codecs, IDR scheduling, capture, AAC and
  receiver LOAD semantics are unchanged. No forced live seeks were added.
- Advertised snapshots change no more than once per second, coalescing pending
  segments. Retirement starts at the actual snapshot removal, not an earlier
  pending publish. Each retired segment remains fetchable until removal time
  plus its duration plus its largest containing advertised-window duration.
- Payload is bounded to 128 MiB / 64 retained entries. Protected segments are
  not silently evicted to meet a cap; admission fails explicitly. Segment
  duration accounting uses integer microseconds and six-decimal EXTINF text.
  Publication age is visible in diagnostics instead of pretending stalled
  production is fresh.
- Added fake-clock tests for short/variable segment durations, burst commits,
  fractional retirement boundaries, protected-memory pressure, a delayed live
  client over 20 simulated minutes, and timing precision over a simulated hour.
  Actual loopback HTTP requests test an old URI just before and after expiry.
- Extended the real H.264/HLS/HTTP/motion-decode and AAC A/V transport tests to
  eight seconds of media. Kept exact decoded audio counts, per-segment video
  decode and motion assertions. Initial-buffer wait is bounded to 30 seconds;
  the GUI now explains the eight-second prebuffer for Cast only.

Hardware validation: `target/release/cermin-cli.exe cast --target <TV> --test
--duration 360` used the DLL encoder and generated picture/tone (no real desktop
or audio capture). It encoded 10,791 frames over 359.7 seconds at 30 fps and
produced 352 A/V segments. The receiver completed 339 segment responses, with
zero missing-segment or failed-write responses. Advertised history stayed near
12 seconds; steady retained payload was roughly 9–10 MB (25–27 segments).
First PLAYING arrived about 26 seconds after startup. One early BUFFERING event
was immediately followed by PLAYING in the received log, then more than five
minutes had no further BUFFERING report and playback time kept advancing.
The command and follow-up probe exited 0; the receiver returned to idle.

This fixes reproducible server-contract defects and improves the observed
longer-run telemetry. It is not visual confirmation that all real-desktop
slideshow cases are resolved; receiver rendering, real-content A/V behavior and
startup latency still require user feedback.

Verification: required `cargo fmt --all -- --check`, locked workspace Clippy,
workspace build and default/GUI test suites, and the DLL/GUI build all passed
(existing warnings remain). Default suite: **296 passed, 4 ignored**; GUI suite:
**307 passed, 4 ignored**. The extended eight-second moving-frame HLS/HTTP/decode
test also passed using the real OpenH264 DLL by running the compiled ignored
test with `dist/` as its working directory (the DLL is present there). Release
GUI startup and graceful window close, plus CLI `probe`, passed. The optimized
GUI/CLI and matching DLL are in `target/release`; `dist/` was not repackaged.
No Linux runtime or new real-desktop long-duration run was performed here.

### Cast slideshow investigation and encoding headroom (2026-09-25)

- Investigated the report of desktop still images changing about every five
  seconds. That is not the intended behavior of the HLS video stream; delayed
  playback is different from a low visible frame rate. The exact original panel
  symptom has not been visually reproduced/verified by the agent.
- Added `cast-benchmark --display <n> --duration <1..60> [--test]`, reusing the
  Cast capture/scale/H.264/mux producer without audio capture, a receiver, HTTP
  serving or media files. Test mode uses the same 1280x720 source as live Cast.
  Reports include actual backend/dimensions, encoder/build type, FPS, actual IDR
  NALs, segment counts, exclusive stage timings and sampled input-change counts.
- Added bounded, numeric HTTP delivery statistics and owned-session media-status
  polling. No token-bearing URLs, media bodies or pixel samples are logged.
  Counters describe completed responses, not proof of panel presentation.
- Fixed the GDI fallback's missing `GdiFlush` before direct DIB memory reads,
  validated bitmap selection and bounded the DIB geometry/allocation. This is a
  Windows API contract fix, not a confirmed explanation for the DXGI-session
  slideshow. Offscreen GDI tests do not capture the desktop.
- Strengthened the HLS encode/HTTP/decode regression with a moving high-contrast
  marker. Each independently decoded segment must show changing interframes,
  and a repeated-picture negative case must fail the motion criterion.
- Rebuilt `target/release/cermin.exe` and `cermin-cli.exe` with the optimized
  DLL encoder, and placed the existing official OpenH264 DLL beside them.
  Feature defaults and AirPlay media behavior are unchanged. `dist/` remains
  the previous package, not the newly rebuilt deliverable.

Measurements on the affected laptop (separate short runs, not a controlled
performance comparison): source-encoder desktop benchmark produced 19.0 fps,
with 34.4 ms mean H.264+I420 time and a 393 ms maximum. The DLL-encoder benchmark
produced 29.7 fps (298 frames/10 s), 11.6 ms mean H.264+I420, 29.6 ms maximum,
and 253 sampled input changes. Both used DXGI/Intel, 1920x1080 downscaled to 720p.
Different desktop activity/load can affect these figures.

A bounded DLL-based real-desktop/system-audio session then produced 892 video
frames over 29.8 s (~29.9 fps), 28 A/V segments and a maximum capture gap of 60.4 ms.
The TV fetched 25 segments with no missing-segment or failed-write responses;
its reported playback time advanced from about 2.99 s to 17.76 s without another
BUFFERING report after startup. The mostly static desktop had few sampled
changes during that run. The command exited 0 and a subsequent status probe
showed the receiver idle. These establish producer/delivery progress, **not** a
verified fix for visible slideshow behavior. User motion/lip-sync confirmation
is still required; the HLS buffering delay remains.

Verification for this follow-up:

- Required formatting, locked workspace Clippy/build/tests, GUI tests and
  DLL/GUI build all passed. Existing warnings remain. Default workspace tests:
  **285 passed, 4 hardware tests ignored**; GUI-enabled: **295 passed, 4 ignored**.
- The source-encoder moving-frame HLS/HTTP/decode regression passed. The DLL
  variant initially failed under `cargo test` because its test working directory
  lacked the DLL. Running the same compiled ignored test from `dist/`, where the
  official DLL is present, passed without changing the test or its assertions.
- The rebuilt DLL-encoder release GUI became ready and exited after a normal
  window close; CLI `probe` and a 1280x720 synthetic local benchmark passed.
  No Cermin session/process was left running by these checks.
- No native low-latency Cast Streaming implementation or HLS buffer-size change
  was made. Sustained receiver presentation and this user's slideshow symptom
  still need visual confirmation rather than treating PLAYING/FPS as proof.

### Hybrid-GPU capture fallback and diagnostics (2026-09-26)

The user hit `DuplicateOutput: ... (0x887A0004)` connecting from an Intel +
NVIDIA RTX 4050 laptop. This is the documented Microsoft Hybrid limitation:
Desktop Duplication is unsupported against the discrete GPU, and the laptop's
panel output was enumerated through the NVIDIA adapter.

- Added `hybrid.rs`: reads/merges the per-app Windows GPU preference
  (`HKCU\Software\Microsoft\DirectX\UserGpuPreferences`, `GpuPreference=1`) while
  preserving other applications' entries. The preference applies to the next
  process, so the session message tells the user to restart for DXGI speed.
- Added `gdi.rs`: a `BitBlt`-based capture backend that locates the monitor via
  DXGI (which still enumerates) and captures pixels regardless of GPU
  assignment. Slower than duplication, but it makes capture work on hybrid
  laptops, Remote Desktop and some VMs.
- `create_capture_backend` now selects `auto` (DXGI, then GDI on the documented
  duplication refusal), or a forced backend via the new documented
  `CERMIN_CAPTURE_BACKEND=auto|dxgi|gdi`.
- DXGI duplication failures name the adapter and the hybrid limitation; the
  fallback also logs the integrated-GPU request. `DisplayInfo` reports the
  owning adapter, `cermin-cli displays` prints it, and the new
  `cermin-cli capture-probe --display <n>` grabs three frames and reports the
  active backend.
- Tests: preference-merge unit tests, a registry round trip in a scratch key,
  backend-preference parsing, and an ignored Windows smoke test that exercises
  the forced GDI backend. Hardware capture still requires a real desktop.
- Follow-up refinements: the preference is pre-applied to sibling
  `cermin.exe`/`cermin-cli.exe`, capture makes console builds per-monitor DPI
  aware (so GDI captures the full 1920x1080 instead of a virtualized
  1536x864), and a one-shot notice is surfaced in the GUI log when the fallback
  is active.

Hardware evidence on the affected Lenovo (Intel UHD + RTX 4050, Windows 11
25H2): `cermin-cli capture-probe` first reported the DXGI refusal on the NVIDIA
adapter, set the preference and captured 1920x1080 via GDI (19-60 ms/frame);
the next process start reported `backend: dxgi` on the Intel adapter at
8-10 ms/frame. A 30-second real-desktop Cast session with system audio reached
receiver PLAYING, exited 0 and left the receiver idle. The release GUI
preference is set; the GUI binary itself still needs a rebuild/restart for the
user to pick up the fix.

### Google Cast system audio and A/V timing (2026-09-25 follow-up)

The user confirmed the initial video connection test appeared correctly on the
TV. This follow-up adds Windows system audio, not low-latency Cast Streaming.

- Cast now defaults to system audio on Windows, with a GUI **System audio (AAC)**
  checkbox and CLI `--no-audio` fallback. Linux remains video-only. AirPlay's
  existing capture, ALAC and mute behavior is unchanged.
- Added timestamped WASAPI loopback of the default playback endpoint (not the
  microphone). The Cast worker never invokes volume/mute APIs. A calibrated
  per-session QPC/Instant origin aligns capture timestamps with video PTS;
  resampler phase, queue losses, discontinuities and negative startup samples
  retain their timeline positions. Capture and PCM queues are bounded.
- Added a thread-affine Media Foundation AAC-LC encoder: 44.1 kHz stereo,
  128 kbps, 1024-sample blocks, ADTS framing and rational timestamps. It uses
  the already-pinned Windows bindings and installed Windows codecs; no new
  registry versions or external runtime executables. COM/MF samples, buffers
  and event collections have explicit error-path ownership.
- HLS now multiplexes AAC and H.264 with distinct PIDs, correct PMT/CRC, shared
  PTS/PCR offset and per-PID continuity. Audio on an IDR timestamp enters the
  new segment; earlier audio is present before the previous segment is sealed.
  Additional PCR-only packets keep the clock advancing during slow video and
  repeat the previous payload counter instead of consuming the next one.
- PCM is assembled on an absolute sample grid, with silence for missing data
  and a 100 ms capture-arrival allowance. Bounded audio catch-up is serviced
  before and after video work, preventing low video FPS from steadily starving
  audio. Track queues are timestamp-ordered and fail on excessive lag rather
  than growing indefinitely.
- `cast --test` now generates quiet 440 Hz pulses and a clock-aligned white
  marker on Windows, without opening a capture endpoint or desktop. Tests
  exercise real AAC encode/decode, combined H.264/AAC TS demux with exact decoded
  sample counts, concurrent codec instances, timestamp math, segment boundaries,
  queue limits, cancellation and video-only/AirPlay compatibility.
- Review corrected per-stream Media Foundation status handling, PCM-vs-AAC
  frame-count units, output-buffer bounds and clock calibration. An intermediate
  test's factor-of-two sample-count failure was stereo-frame vs interleaved-value
  arithmetic; the corrected transport assertion is exact, not relaxed.

Current verification: formatting, locked workspace Clippy/build/tests, GUI tests,
and DLL/GUI compilation passed. Default workspace tests: **262 passed, 3 ignored**;
GUI-enabled workspace tests: **272 passed, 3 ignored**. Existing warnings remain.
The hardware tests are ignored in ordinary test runs. Additional checks:

| Check | Result |
|---|---|
| `cargo build --locked --release -p rotten-app --features gui --bins -j 2` | Passed; updated source-encoder GUI/CLI are in `target/release`. `dist/` was not repackaged. |
| `cargo test --locked -p rotten-app --lib audio::timed::tests::real_loopback_timed_smoke -- --ignored` | Passed on the real Windows playback endpoint: local timestamped loopback start/packet/stop, no mute, saved audio or network transmission. |
| Release GUI startup/close, CLI `probe` and `cast --help` | Passed; the GUI window became ready and exited on close. No desktop session launched through the GUI. |
| Real Skyworth synthetic A/V session, `cast --test --duration 30` | Receiver reported PLAYING; 785 synthetic video frames, 29 published A/V segments, 1,310,720 PCM sample frames submitted. Exit code 0; status probe two seconds later showed no applications (idle). No real screen or system audio transmitted. |

The user must still confirm audible output and perceived lip-sync on the TV.
Real system-audio-to-TV playback, endpoint changes, long-session drift/recovery,
other receivers, native low-latency Cast and Linux compilation/runtime have not
been established by these checks. This is implementation review, not an
independent audit.

### Google Cast live desktop video (2026-09-25)

- Added the separate `rotten-cast` crate: bounded Cast V2 protobuf framing over
  TLS, receiver status, Default Media Receiver launch, HLS LOAD, heartbeat,
  playback-state reporting and session-scoped STOP. Partial reads/writes survive
  cancellation. Broadcast statuses and request/source correlation are covered.
- Added `CastDevice` / `ReceiverDevice`, `_googlecast._tcp` discovery (decimal
  capability TXT parsing, video filtering and scoped daemon lifetime), mixed
  GUI discovery, protocol labels and manual targets. Cast bypasses AirPlay
  credential/PIN handling; AirPlay connection/media code and defaults are intact.
- Added `cast-probe`, `cast`, `discover --protocol`, and an optional cooperative
  `cast --duration` deadline. GUI hides AirPlay-only PIN/volume controls for Cast
  and reports receiver PLAYING instead of claiming a verified on-screen image.
- Cast reuses existing capture/OpenH264, fits video inside 720p at a target
  30 fps / 4 Mbps, and serves live video-only HLS through Google's built-in
  receiver. This is **not native low-latency Cast Streaming**. No audio capture,
  mute changes, FFmpeg, Chrome bridge or Cermin TV app are involved.
- MPEG-TS segments start on actual IDRs with SPS/PPS; PAT/PMT CRCs, PES PTS/PCR
  and continuity across segments are tested. HLS and HTTP data are bounded,
  tokenized, memory-only, restricted to the selected LAN interface/receiver IP,
  and stopped with session ownership. HTTP parsing and task shutdown have
  boundary/cancellation tests.
- Security limitation: Cast TLS verifies handshake signatures, but not the
  self-signed receiver's identity. Media is plaintext LAN HTTP. README and UI
  warn to use a trusted LAN; no general TLS trust settings were weakened.
- Review corrections included broadcast handling, cancellation-safe writes with
  whole-write deadlines, no STOP after media/transport replacement, cleanup
  ownership checks, cross-segment continuity, exact memory/header bounds and
  cancellation while waiting for initial HLS segments.
- Added real synthetic encode → HLS → loopback HTTP → independent demux/decode
  coverage. A fresh OpenH264 decoder decodes each of three segments independently
  (90 frames total). Ordinary tests do not contact a TV or capture the desktop.

Hardware evidence: mDNS discovered the user's Skyworth SWTV-22AE-FHD at
its current address (different from the older screenshot), and the Cast status
probe successfully read its idle Backdrop application. Bounded 30-second
synthetic-only Cast sessions launched the Default Media Receiver and exited
without error. The optimized source-encoder run explicitly reported PLAYING,
encoded 739 synthetic frames and published 29 segments. A probe immediately
after STOP still saw the app while it was closing; a later probe confirmed the
receiver was idle with no applications. STOP transmission is not a synchronous
acknowledgement of the TV's application shutdown.
No real desktop/audio was captured. Visual presentation, smoothness and actual
latency still require the user's observation; this is not full hardware acceptance.

Verification on Windows with Rust 1.98.1 (2026-09-25):

| Check | Result |
|---|---|
| `cargo fmt --all -- --check` | Passed after final edits. |
| `cargo clippy --locked --workspace --all-targets` | Passed with existing warnings; not warnings-as-errors. New Cast crate also passes its targeted Clippy check. |
| `cargo build --locked --workspace --all-targets` | Passed. |
| `cargo test --locked --workspace --all-targets` | 195 passed, 2 hardware tests ignored. Includes real source encode/HTTP/decode and existing AirPlay regressions. |
| `cargo test --locked --workspace --all-targets --features rotten-app/gui` | 202 passed, 2 hardware tests ignored, including GUI Cast/PIN dispatch and session shutdown tests. |
| `cargo build --locked -p rotten-app --no-default-features --features encode-dll,gui --bins` | Passed; DLL runtime streaming was not tested in this addition. |
| `cargo build --locked --release -p rotten-app --features gui --bins -j 2` | Passed; initial unrestricted-parallel attempt exceeded the command time limit. Source-encoder GUI/CLI are in `target/release`, no Cast DLL/helper needed. Existing `dist/` was not repackaged. |
| Release GUI startup/close and CLI `probe` / `cast --help` | Passed: window became ready and process exited after close. No session was started through the GUI. |
| Real Skyworth discovery, TLS/status, synthetic Cast playback and STOP | Passed to receiver-reported PLAYING and eventual idle; no screen/audio capture. Panel appearance and latency were not observed. |

Linux compilation/runtime, other Cast receivers, real-desktop Cast acceptance,
long-session recovery, Cast audio, low-latency Cast Streaming and authenticated
receiver identity remain unverified or unimplemented as described in README.
This was implementation review, not a separate independent audit.

### Session lifetime and shutdown

- Added `ScopedTask` in `crates/rotten-core/src/task.rs`. Session-owned tasks are
  cancelled when their owner is dropped, including failed RTSP setup attempts.
- App tasks use a JoinSet. Timing, event connections and the diagnostic data
  reader now have explicit owners instead of detached task handles.
- Pairing, connection setup and streaming observe the stop flag. Stop interrupts
  a blocked video operation; the partially written socket is discarded.
- Capture initialization finishes before local audio starts or is muted, avoiding
  early capture errors that previously bypassed awaited audio cleanup.
- Streaming errors preserve their original cause even if audio cleanup also fails.
- Ctrl+C requests cooperative CLI cleanup. Discovery and retry waits also stop.
  Runtime shutdown is bounded for already-running blocking calls/PIN reads.
- GUI window close joins the worker. Starting precedes session events, and Stopped
  is delivered after releasing the finished session, preventing ignored reconnects.
  GUI worker launch failures and panics are reported.

### Video correctness and transport

- Dropping a frame producer wakes its receiver; the final queued frame is drained
  before EOF. Queue overwrite counters are finalized on normal stream exit.
- Video packets and heartbeats have a 30-second write deadline; stop is checked
  during socket operations. The convenience connection handles IPv6 addresses.
- First-frame readiness follows the first successfully written VCL frame, even
  when an earlier encoder output contained only codec data. Failed writes do not
  signal readiness or count as sent frames.
- Source and DLL encoder features can coexist without duplicate function definitions.
- `crates/rotten-video/tests/stream_pipeline.rs` sends real encoded synthetic RGBA
  over local TCP, parses codec/VCL packets, decodes H.264, checks timing/readiness
  and statistics, then verifies EOF after producer closure. It supports an explicit
  DLL run; DLL mode is ignored by default because the runtime DLL is required.

### Audio reliability

- `AudioMirror::start` returns a consistent handle/PCM-receiver pair on all platforms.
- Windows startup waits for WASAPI readiness and reports initialization failures.
  Stop reports worker errors and panics instead of discarding them.
- RAII restores the original mute state and releases COM, format memory and the
  started audio client on normal exit, errors and unwinding.
- The PCM queue cannot block shutdown when full. A closed consumer stops capture,
  including while no packets are arriving. Format validation precedes PCM decoding.
- Tests cover mute restoration, retry after a failed restore, an untouched endpoint,
  invalid formats, PCM conversion, resampling, Drop, and worker failures.

### RTSP response handling

- Added connection/response deadlines, 64 KiB header and 8 MiB body limits, and
  validation for Content-Length, truncated bodies and oversized HAP frames.
- Partial plaintext/encrypted responses survive cancelled reads. Coalesced replies
  retain their extra bytes instead of discarding the following response.
- Loopback tests cover fragmentation, cancellation/resume, encrypted multi-frame
  replies, coalesced responses, invalid lengths and authentication-tag failures.

### Saved pairing and build checks

- Credentials are written and synced to a sibling temporary file, then replaced.
  Failed replacement preserves the destination and removes temporary files.
- Existing Unix parent-directory permissions are no longer changed for custom
  credential paths; credential files use mode 0600. Bare filenames work correctly.
- Tests cover replacement, locked-file failure on Windows, temporary-file cleanup,
  relative path handling and Unix file/parent permissions.
- CI now includes native Windows source/GUI tests, DLL/GUI builds and combined
  features. CLI checks name their binary explicitly.
- Release scripts use locked builds, HTTPS, temporary DLL extraction, explicit
  Windows x64 targeting, custom target-directory support and checked exit codes.
  Go environment variables are restored; MinGW runtime DLL lookup is portable.
- README requires Rust 1.95 based on locked GUI dependencies and corrects CLI/probe
  instructions. Existing workspace formatting drift was normalized with rustfmt.
- The unused vendored PlayFair `printf`/`fprintf` stubs were removed: `PLAYFAIR_QUIET`
  replaces every call site, and the definitions clashed with MinGW's stdio headers.
  The `x86_64-pc-windows-gnu` cross-build compiles again.

### Display selection

- `cermin-cli displays` lists capturable monitors with index, name, resolution
  and virtual-display flag; `DisplayInfo::label` has a unit test.
- The GUI enumerates displays at startup (refreshable) and shows a "Screen to
  mirror" picker; the selected index is passed into the session config.
- `--display` is documented in the README and defaults to the primary display.

## Verification evidence

Windows checks used Rust 1.98.1 and Visual Studio 2026 Build Tools. Rust was installed
under `%TEMP%/cermin-rust` without changing the user profile or permanent PATH.
Logs are in ignored `check-*.log` files in the workspace.

| Check | Result and scope |
|---|---|
| `cargo test --locked --workspace --all-targets --features rotten-app/gui` | 64 tests passed, including GUI shutdown, locked credentials, protocol regressions and source encode/TCP/decode. Hardware smoke tests are ignored by default. |
| `cargo build --locked --workspace --all-targets --features rotten-app/gui` | Passed. |
| `cargo clippy --locked --workspace --all-targets --features rotten-app/gui` | Passed; style/dead-code warnings remain. This is not a warnings-as-errors result. |
| `cargo fmt --all -- --check` | Passed. |
| `cargo build --locked -p rotten-app --no-default-features --features encode-dll,gui --bins` | Passed. |
| `cargo check --locked -p rotten-app --all-targets --all-features` | Passed, including combined encoder features. |
| `cargo check --locked --workspace --all-targets --target x86_64-unknown-linux-gnu` | Passed using checksum-verified Zig 0.16.0 C/C++ wrappers and the Linux Rust standard library. This compiles Linux and Unix-specific tests; it does not run them. |
| `cargo test --locked -p rotten-video --features software-encode-source,software-encode-dll --test stream_pipeline -- --ignored` | Passed with the official OpenH264 2.6.0 Windows DLL; actual DLL encode/TCP/decode exercised. |
| `cargo test --locked -p rotten-capture --test desktop_smoke -- --ignored` | Passed: three complete RGBA frames from the actual Windows desktop. No screenshots saved. |
| `cargo test --locked -p rotten-app --lib real_loopback_starts_and_stops_without_muting -- --ignored` | Passed against the local WASAPI endpoint; no muting or audio files. |
| GUI startup and window close | Confirmed the Cermin window becomes ready and the process exits after close. CLI help, version and local startup probe also passed. |
| Release-script checks | PowerShell parser, Bash syntax and four isolated scenarios passed: Cargo failure, probe failure, real HTTPS/Python DLL extraction with mocked build outputs, and Go failure/environment restoration. |
| `bash ./scripts/build-windows.sh` (real MinGW cross-build) | Passed with WinLibs GCC 16.2.0 (UCRT) and Go 1.27.1: both executables, the OpenH264 DLL and `fpsap-helper.exe` were produced, and the cross-built `cermin-cli.exe` reports 0.1.2 with a passing `probe`. |

The DLL was downloaded from Cisco over HTTPS. SHA-256 of the tested file:
`2076cb5675ec6c1a4c70e7a2a322552f547b6eeed649d6dfcd9e02a543b24691`.
A copy is beside the debug executable and another beside the integration tests.

## Remaining limitations and next work

- No Samsung TV/Apple TV session was run. Receiver pairing, encrypted-video
  interoperability, A/V synchronization, TV volume, Wi-Fi recovery and visible
  presentation quality still need an actual receiver.
- WASAPI startup/stop was tested without muting. Actual mute/loopback behavior,
  endpoint removal and driver hangs still need hardware acceptance testing.
  The mute fallback is heuristic; drivers delivering no packets need special care.
- Already-running native blocking calls cannot be forcibly cancelled by Tokio.
  Bounded runtime shutdown prevents them from holding process exit indefinitely.
- RTSP response reads and connection establishment are bounded; legacy control
  writes still lack their own write deadline.
- Credential replacement tests do not establish power-loss durability: the
  containing directory is not fsynced. Windows credentials use filesystem access
  controls; this change does not add encryption at rest.
- Capture dimensions no longer override configured width/height: defaults match
  the captured display, explicit values scale it down (never up), and auto bitrate
  and presentation size derive from the resolved stream size. Device-specific
  performance measurements are still next work.
- Linux runtime/permissions tests and the 7-Zip extraction branch were not
  executed here. Linux audio capture remains a silence stub; hardware encoders
  remain unimplemented.

## Review and time limit

Five `gpt-6-astra` agents at high reviewed lifecycle, audio, protocol, portability
and video. Two additional `gpt-6-astra` agents at Ultra audited app reliability and
protocol/video validation. All seven completed their bounded reviews.

The final user-requested 15-minute work window began at 08:26:34 UTC on 2026-09-19,
with a stop deadline of 08:41:34 UTC. Work stopped within that limit after final
checks and documentation. All review agents finished and no build or test process
was left running. The broader goal is paused at the user's request; remaining
receiver and platform checks are listed above.
