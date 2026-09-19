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
- Capture dimensions still override configured width/height. A consistent
  resolution policy and device-specific performance measurements are next work.
- Linux runtime/permissions tests, the real MinGW cross-build, full release
  packaging, the 7-Zip extraction branch and hosted CI were not executed here.
  Linux audio capture remains a silence stub; hardware encoders remain unimplemented.

## Review and time limit

Five `gpt-6-astra` agents at high reviewed lifecycle, audio, protocol, portability
and video. Two additional `gpt-6-astra` agents at Ultra audited app reliability and
protocol/video validation. All seven completed their bounded reviews.

The final user-requested 15-minute work window began at 08:26:34 UTC on 2026-09-19,
with a stop deadline of 08:41:34 UTC. Work stopped within that limit after final
checks and documentation. All review agents finished and no build or test process
was left running. The broader goal is paused at the user's request; remaining
receiver and platform checks are listed above.
