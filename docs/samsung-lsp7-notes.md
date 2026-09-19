# Samsung Projector LSP7 — AirPlay Mirroring Investigation Summary

**Date:** 2026-09-18 (evening, Europe/Amsterdam timezone offset in logs)
**Author:** opencode sessions (Samsung projector casting, `ses_f4bf6c564ffeLrcAlKCs1Iovrg`)
**Status at pause:** Major breakthrough reached — receiver accepts the mirroring SETUP request only when
`timingProtocol: PTP` is used. Response decoding was the very next step. Code changes are **uncommitted**
in the Cermin repo.

---

## 0. RESOLVED — 2026-09-19: screen mirroring + system audio both work

**Result:** the real PC screen mirrors to the LSP7 **with PC system audio** (`--audio` flag).
The last piece was the audio path; final working recipe below (video items 1-5, audio 6-9).

### Video (works without `--audio`)

1. **Projector moved to the home Wi-Fi** (now `192.168.1.179`; AirPlay port is dynamic — always get it
   from `cermin.exe discover --timeout 8`).
2. **Dynamic `timingProtocol`** (`device.rs`): PTP when bit 41 set and bit 45 clear (`timing_protocol()`).
3. **Sender participates in PTP**:
   - `timingPeerInfo`/`timingPeerList` in the SETUP plists: `ID` (session UUID), `Addresses`, `ClockID`
     (64-bit identity = DACP identifier `8e70f5df22738290`), `DeviceType: 0`.
   - `SETPEERS` right after the session SETUP: body is a **bare plist array of IP strings**
     `[receiver_ip, our_ip]` (not peer dicts) + an immediate timing kick.
   - `ptp.rs`: two-step Sync/Follow_Up (125 ms) + Announce/Signaling (1 s) on UDP 319/320, answers
     Delay_Req.
   - **The LSP7 asserts itself as PTP master**: it replies with Announce/Signaling and its own
     Sync/Follow_Up. The sender slaves: Follow_Up origin timestamp (+correction) sets a session offset
     (`rotten-core/src/ntp.rs::set_session_offset_ns`) so all timestamps are in the receiver's clock
     domain.
4. **Stream order** (cliairplay/Samsung quirk): session SETUP (no streams) → SETPEERS → connect event
   port → RECORD → audio stream SETUP (type 96) → video stream SETUP (type 110). Receivers 200-ACK
   everything but never start streams attached before RECORD.
5. **Video cipher = ChaCha20-Poly1305**, key = HKDF-SHA512(pair-verify shared secret,
   salt `DataStream-Salt<videoStreamID>`, info `DataStream-Output-Encryption-Key`); shk/shiv (first 16 B
   of HAP Control-Write/Read keys) still ride the video stream descriptor.

### Audio (needs `--audio`; silence stream otherwise)

6. **Audio stream** (type 96): `shk` = first 32 bytes of the pair-verify shared secret; descriptor also
   carries the sender's `controlPort` **and `dataPort`** (missing dataPort → receiver drops audio).
7. **ALAC packets**: use the `alac-encoder` crate (Rust port of Apple's ALACEncoder) — the hand-rolled
   verbatim encoder produced loud digital buzzing (wrong header bits / bogus 32-bit frame size).
   352-sample frames, 44.1 kHz, stereo S16.
8. **RTP**: 12-byte header (PT 0x60, SSRC 0), ChaCha20-Poly1305 with nonce = LE64(packet counter)
   (trailer = 16-byte tag + 8-byte nonce), AAD = header[4..12]. Pacing must be **elapsed-time based
   with catch-up sends**: Windows timer granularity (~15 ms) starves the receiver at a plain 7 ms
   ticker (~80 pkt/s instead of 125).
9. **PTP anchors** (28-byte 0x90d7/0x80d7): RTP ts, PTP wall ns, `frame_1 = play_pos + 11035`,
   `frame_2 = frame_1 + 77175`, master clock identity (the receiver's own PTP id, captured from its
   Follow_Up). The anchor line is frozen at the first sync with **lead**: `play_pos = pos0 +
   (wall - wall0)·rate − lead_frames` (lead = 22050 = 500 ms). Without the lead the receiver treats
   every sample as late and drops everything; first anchor 0x90, later ones 0x80, repeat every 500 ms.
10. **Capture** (`rotten-app/src/audio/capture.rs`): WASAPI loopback of the default render endpoint
    (48 kHz float32) → linear resample to 44.1 kHz → S16 stereo → mpsc → RTP loop. Windows-only.

**Run commands (current):**
```powershell
# find the current dynamic AirPlay port
& dist\cermin.exe discover --timeout 8
# mirror with system audio
dist\cermin.exe mirror -t 192.168.1.179 --port <PORT> --width 1280 --height 720 --fps 30 --audio --debug
# test pattern instead of capture: add --test
```

Debug env vars: `CERMIN_AUDIO_TONE=1` (440 Hz instead of capture),
`CERMIN_NO_AUDIO_RTP=1` (disable audio), `CERMIN_NO_AUDIO_SYNC=1` (disable anchors),
`CERMIN_DEBUG_LOG=1` (JSON trace to `%TEMP%\cermin-debug.log`).

**Key files changed for the resolution:** `crates/rotten-core/src/device.rs` (timing protocol helpers),
`crates/rotten-core/src/ntp.rs` (session offset), `crates/rotten-protocol/src/ptp.rs` (new PTP engine),
`crates/rotten-protocol/src/mirror_rtsp.rs` (peer info, SETPEERS, stream order, ChaCha selection,
audio gating), `crates/rotten-protocol/src/airplay_conn.rs` (`local_addr`, `rtsp_set_peers`),
`crates/rotten-protocol/src/audio_rtp.rs` (ALAC/RTP/PTP anchors), `crates/rotten-app/src/audio.rs` +
`crates/rotten-app/src/audio/capture.rs` (WASAPI loopback), `crates/rotten-app/src/mirror.rs` (wiring),
`Cargo.toml`s (alac-encoder, windows audio features).

---

## 0b. Earlier findings from this session (historical, kept for context)

**Result:** the test pattern and then the real PC screen mirrored to the LSP7. What made it work:

1. **Projector moved to the home Wi-Fi** (now `192.168.1.179`; AirPlay port is dynamic — always get it
   from `cermin.exe discover --timeout 8`).
2. **Dynamic `timingProtocol`** (`device.rs`): PTP when bit 41 set and bit 45 clear (`timing_protocol()`).
3. **Sender participates in PTP**:
   - `timingPeerInfo`/`timingPeerList` in the SETUP plists: `ID` (session UUID), `Addresses`, `ClockID`
     (64-bit identity = DACP identifier `8e70f5df22738290`), `DeviceType: 0`.
   - `SETPEERS` right after the audio SETUP: body is a **bare plist array of IP strings**
     `[receiver_ip, our_ip]` (not peer dicts) + an immediate timing kick.
   - `ptp.rs`: sends two-step Sync/Follow_Up (125 ms) + Announce/Signaling (1 s) on UDP 319/320 and
     answers Delay_Req.
   - **The LSP7 asserts itself as PTP master**: it replies with Announce/Signaling and then its own
     Sync/Follow_Up. The sender slaves: Follow_Up origin timestamp (+correction) sets a session offset
     (`rotten-core/src/ntp.rs::set_session_offset_ns`) so all video timestamps are in the receiver's clock
     domain (observed master time ≈ receiver uptime, e.g. 20567 s).
4. **RECORD before the video stream SETUP** (Samsung quirk; cliairplay: receivers 200-ACK everything and
   render nothing otherwise).
5. **Video cipher = ChaCha20-Poly1305**, key = HKDF-SHA512(pair-verify shared secret,
   salt `DataStream-Salt<videoStreamID>`, info `DataStream-Output-Encryption-Key`); shk/shiv (first 16 B of
   HAP Control-Write/Read keys) still ride the video stream descriptor.
6. **(Superseded)** Legacy audio RTP initially broke the session (\write vcl packet: os error 10054\);
   that ended once the section 0 audio items (ALAC via alac-encoder, PTP anchors with lead, dataPort,
   catch-up pacing, stream order) were implemented. Kept here only to explain why intermediate builds
   disabled audio on PTP-only receivers.
**Run commands (current):**
```powershell
# find the current dynamic AirPlay port
& dist\cermin.exe discover --timeout 8
# mirror (video only for now)
dist\cermin.exe mirror -t 192.168.1.179 --port <PORT> --width 1280 --height 720 --fps 30 --debug
# test pattern instead of capture: add --test
```

**Key files changed for the resolution:** `crates/rotten-core/src/device.rs` (timing protocol helpers),
`crates/rotten-core/src/ntp.rs` (session offset), `crates/rotten-protocol/src/ptp.rs` (new PTP engine),
`crates/rotten-protocol/src/mirror_rtsp.rs` (peer info, SETPEERS, RECORD order, ChaCha selection, audio
gating), `crates/rotten-protocol/src/airplay_conn.rs` (`local_addr`, `rtsp_set_peers`).

---

## 1. Goal

Mirror this Windows PC's screen to a **Samsung Projector LSP7** over AirPlay from a custom sender
(the Cermin project), not via Samsung's own apps or Windows "Cast to Device".

---

## 2. Target / environment

| Item | Value |
|---|---|
| Device | Samsung Projector LSP7 (`model: LSP7`, `manufacturer: Samsung`) |
| IP | `192.168.137.247` (network `192.168.137.x`, PC is `192.168.137.1`) |
| AirPlay port | **dynamic** — observed `41089`, `41161`, `59572`, finally **`47439`** (changes when AirPlay is toggled off/on) |
| Receiver stack | `Server: AirTunes/377.25.06`, `sdk: AirPlay;2.3.1-f.1`, `sourceVersion: 377.25.06`, `firmwareRevision: T-KTSU2UABC-2743.0` |
| `/info` IDs | `deviceID: BC:7E:8B:76:0E:22`, `pi: 6E:EB:03:4A:4F:33`, `serialNumber: 0A2G3NBNB00049A` |
| Display | 1920×1080, `statusFlags: 132`, `protocolVersion: 1.1` |
| PTP | `PTPInfo: OpenAVNU ArtAndLogic-aPTP-changes ... Sep 22, 2018` |
| Windows | Windows 11 25H2 (build 26200, registry ProductName still says Windows 10) |
| Sender project | Cermin repo (Rust workspace, binary `cermin.exe`) |
| Credentials | `%APPDATA%\cermin\credentials.json` (device `BC:7E:8B:76:0E:22`, `hap: true`) |
| AirPlay custom code | **1234** (set on projector: Settings → General → Apple AirPlay Settings → Require Code → custom code) |

### Feature flags (`features = 255521305393072848` = `0x038bcb46007f8ad0`)

Set bits: `[4, 6, 7, 9, 11, 15, 16, 17, 18, 19, 20, 21, 22, 33, 34, 38, 40, 41, 43, 46, 47, 48, 49, 51, 55, 56, 57]`

Critical bits:

| Bit | Name | Set? | Meaning |
|---|---|---|---|
| 7 | ScreenMirroring | ✅ | Mirroring supported |
| 12 | FPSAPv2p5_AES_GCM | ❌ | No FairPlay AES-GCM |
| **14** | **MFiSoft_FairPlay (FP SAP 2.5)** | **❌** | **No FairPlay at all → `/fp-setup` returns 404** |
| 27 | LegacyPairing | ❌ | HAP-only pairing |
| 38 | ControlChannelEncrypt | ✅ | HAP encrypted control channel |
| 40 | BufferedAudio | ✅ | |
| **41** | **PTPClock** | **✅** | **PTP timing** |
| 43 | SystemPairing | ✅ | |
| 45 | NTPClock | ❌ | **NTP timing NOT supported** |
| 46 | HomeKitPairing | ✅ | HAP pairing |
| 47 | PeerManagement | ✅ | |
| 48 | TransientPairing | ✅ | |
| 49 | AirPlayVideoV2 | ✅ | |
| 51 | MfiPairSetup | ✅ | |

---

## 3. What was tried, in order

### 3.1 Windows "Cast to Device" (DLNA) — dead end
- Network profile was **Public** → Windows blocks network discovery. Discovery services
  (`FDPhost`, `FDResPub`, `upnphost`, `WMPNetworkSvc`) were **stopped**; SSDP M-SEARCH got no responses.
- `MiracastPowerManager.exe` crashed **7 times** between 19:12 and 20:18 with
  `Exception code: 0xc0000374` (heap corruption in `ntdll.dll`) — related to casting attempts, not VS Code.
- Conclusion: not viable; moved to AirPlay.

### 3.2 Samsung Tizen REST API (ports 8001/8002)
- `GET /api/v2/` works (device info: `TokenAuthSupport: True`, `PowerState: on`, `resolution: 3840x2160`).
- `/api/v2/applications`, `/api/v2/apps` → **404**; this firmware/projector doesn't expose app control.
- Conclusion: not usable for mirroring.

### 3.3 AirPlay pairing via Cermin
- First attempts used legacy `pair-setup-pin`; HAP `pair-setup` had an **SRP bug** (M1 reply parsed as
  `len=10`, M2 failed) which was fixed in `crates/rotten-crypto/src/srp.rs` / `hap.rs`.
- Repeated failures triggered HAP **BackOff**: `pairing error: M2 pairing error TLV: 3`
  (`TLV 3` = BackOff). Fix: wait, or toggle AirPlay off/on on the projector (resets the counter).
- **Working pairing path:** HAP pair-setup with the custom code `1234`:
  `pair-pin-start 200` → `M1 200 len=409` → `M2 ok` → `M4 server proof OK` → `M6 ok: accessory_id=17B ltpk=32B`.
  Credentials were saved at 22:14:28.
- After pairing, every session starts with HAP `pair-verify` (M1/M3) which succeeds and enables the
  encrypted control channel (ChaCha20-Poly1305 HAP framing).

### 3.4 FairPlay `fp-setup` — 404 (expected)
- `/fp-setup` (RTSP/1.0, `X-Apple-ET: 32`) on the encrypted channel returns **HTTP 404, len 0**.
- Confirmed this means "not implemented", matching feature bit 14 being clear. Same behavior class as
  Denon AVRs in the fairplay-sap-core docs (404 = no FairPlay; 403 = not paired).
- Reference fix found in the doubletake project (commit `0ddfeea579b3de738e246020a1270452e6fe3abb`):
  **"Skip FairPlay fp-setup for devices without FeatureFPSAP25 (e.g. Samsung TVs)"**.

### 3.5 The SETUP request mystery
With fp-setup skipped, the sender sent the mirroring `SETUP` and got **no response at all**
(30 s timeout, zero raw bytes). Extensive probing with a new diagnostic tool (`rotten-probe`) found:

| Probe | Result |
|---|---|
| `OPTIONS * RTSP/1.0` | 200 |
| `POST /feedback RTSP/1.0` | 200 |
| `GET /info HTTP/1.1` (over encrypted channel) | 200 + 1335-byte plist |
| `POST /stream` (HTTP and RTSP) | **404** (endpoint not implemented) |
| `POST /auth-setup` | 400 |
| `POST /audioMode` | 404 |
| `SETUP` with **invalid** `sessionUUID` (not a UUID) | **400** |
| `SETUP` with **valid UUID** but `timingProtocol: "NTP"` | **silence** (server waits forever) |
| `SETUP` with valid UUID and **`timingProtocol: "PTP"`** | **HTTP 200 + 397-byte binary plist** ✅ |

Variables ruled out (all still produced 400 or accepted response, not the silence):
- Header/body split across 1 vs 2 HAP frames — irrelevant (`exchange_parts` also worked).
- Request size (814 B up to 1229 B) — irrelevant.
- Stream ID width (32-bit, max u32, 60-bit nanoseconds) — irrelevant.
- `deviceID` / `macAddress` values (0 vs real), `shk` bytes — irrelevant.
- Request count/order — the earlier "first 7 answered, rest ignored" was a red herring caused by NTP silence.

**Root cause: the LSP7 is a PTP-clock receiver (bit 41 set, bit 45 NTP clear). With `timingProtocol: NTP`
the AirTunes server stalls silently; with `timingProtocol: PTP` it immediately answers `SETUP` with a
397-byte binary plist response.**

### 3.6 VS Code sudden exit (for the record)
Not a crash: VS Code's `main.log` shows `update#setState restarting` at 21:55:40 and extension hosts
exited with code 0 — it restarted itself to apply an update. The interrupted opencode session was
resumed as this one.

---

## 4. What actually worked (confirmed working pieces)

1. **Pairing**: HAP pair-setup with custom code `1234` → credentials persisted.
2. **Session auth**: HAP pair-verify on the AirPlay port (47439) → encrypted control channel.
3. **FairPlay bypass**: skipping fp-setup for a bit-14-clear receiver (no more 404 abort).
4. **Mirror key derivation without FairPlay**: `shk`/`shiv` taken from the pair-verify HAP keys
   (first 16 bytes of `Control-Write` / `Control-Read` keys, per doubletake `deriveStreamKeys`).
5. **SETUP accepted**: `timingProtocol: "PTP"` → `HTTP 200` with a binary plist body (contents not yet
   decoded — that was the next step).

---

## 5. Code changes made during the investigation

`git status` shows modified: `Cargo.lock`, `crates/rotten-capture/src/dxgi.rs`,
`crates/rotten-core/src/config.rs`, `crates/rotten-crypto/build.rs`,
`crates/rotten-crypto/src/lib.rs`, `crates/rotten-crypto/src/srp.rs`,
`crates/rotten-crypto/src/srp_modulus.txt`, `crates/rotten-pairing/src/credentials.rs`,
`crates/rotten-pairing/src/homekit.rs`, `crates/rotten-pairing/src/legacy_pin.rs`,
`crates/rotten-protocol/Cargo.toml`, `crates/rotten-protocol/src/airplay_conn.rs`,
`crates/rotten-protocol/src/mirror.rs`, `crates/rotten-protocol/src/pair_verify.rs`;
untracked: `crates/rotten-crypto/src/hap.rs`, `dist/`.
**Changes from this session are a subset of that diff; run `git diff` before switching machines.**

### 5.1 `crates/rotten-protocol/src/mirror.rs`
- Gate fp-setup on feature bit 14:
  ```rust
  let fp = if device.features.supports_fairplay_sap() {
      Some(crate::fp_setup::run_fp_setup_conn(&mut airplay_conn, creds).await?)
  } else {
      debug!(host = %device.host, "receiver does not support FairPlay SAP; skipping fp-setup");
      None
  };
  let setup = setup_mirror_rtsp(..., fp.as_ref(), ...).await?;
  ```

### 5.2 `crates/rotten-protocol/src/mirror_rtsp.rs`
- `setup_mirror_rtsp` now takes `fp: Option<&FairPlaySession>`.
- When `None`: derive `shk`/`shiv` from `pv.hap_keys` (`out_key`/`in_key` first 16 B; random if no HAP).
- `encode_video_setup_plist(..., shk, shiv, ekey: Option<&[u8;72]>, ...)`: root `ekey`/`eiv` only when
  FairPlay exists; `shk`/`shiv` always inside the video stream descriptor when encrypting.
- Force **AES-CTR** cipher when there is no FairPlay (`effective_cipher = AesCtr`), matching doubletake.
- Audio SETUP plist aligned with doubletake for HAP receivers: **removed `streamConnections`**,
  `supportsDynamicStreamID: false` (was `true`), removed `audioFormatIndex`; kept `shk` (32 B),
  `isMedia`, `redundantAudio: 0`, `disableRetransmits: true`.
- Added `pub fn encode_audio_setup_plist_chacha(...)` (exported for the probe).
- **`timingProtocol` is still hardcoded `"NTP"` in both audio and video plists — THIS IS THE NEXT FIX.**

### 5.3 `crates/rotten-protocol/src/airplay_conn.rs`
- Added diagnostics: `exchange()`, `exchange_full()`, `exchange_parts()`, `send()`, `try_read()`.
- 30 s read timeout on HAP reads (returns `RTSP read timeout (30s)` instead of hanging forever).
- Agent logs for raw encrypted bytes read and plaintext frames written (`CERMIN_DEBUG_LOG=1`).

### 5.4 `crates/rotten-protocol/src/lib.rs`
- Exports `hap_pair_verify_conn` and `encode_audio_setup_plist_chacha`.

### 5.5 `crates/rotten-probe` (new diagnostic binary `cermin-probe`)
- Replaces the old stub. Pairs via stored credentials, does HAP pair-verify, then sends candidate
  requests and prints status/headers/plist.
- Modes: single request, split header/body, `Request::Sequence` (multiple requests on one connection).
- `PROBE_REPLAY_HEX=<file>` env var replays captured wire bytes.
- Current constants: `HOST = 192.168.137.247`, `PORT = 47439`, `REPLY_TIMEOUT_SECS = 8`.

### 5.6 Build / run commands used
```powershell
# In the Cermin repo
cargo build --release                                  # app
cargo build --release -p rotten-probe                  # probe
Copy-Item target\release\cermin.exe dist\cermin.exe -Force
& target\release\cermin-probe.exe

# App run (test pattern):
dist\cermin.exe mirror -t 192.168.137.247 --port 47439 --pin 1234 `
  --width 1280 --height 720 --fps 30 --test --debug

# Debug trace file: %TEMP%\cermin-debug.log (requires CERMIN_DEBUG_LOG=1)
```

---

## 6. State at pause

- `dist\cermin.exe` and `target\release\cermin-probe.exe` are built with
  all changes above **except** the `timingProtocol: PTP` fix (not yet implemented).
- The probe was just extended with `fmt_plist()` to pretty-print the **397-byte HTTP 200 plist response**
  to the PTP SETUP; the probe has **not been rebuilt/re-run** since that change.
- The application still sends `timingProtocol: NTP` in both audio and video SETUP plists, so it still
  hangs at the first (audio) SETUP.
- Nothing is committed; VS Code is fine (update restart only). The projector is at
  `192.168.137.247:47439` and responds to `/info`.

---

## 7. Next steps (tomorrow, from a new repo)

1. **Preserve the code**: DONE — the work is committed to the Cermin repository; the
   temporary `airplay-session.patch` file is no longer part of the tree. The `dist/` folder
   is not committed — rebuild with `scripts\build-release.ps1` (or `cargo build --release
   --no-default-features --features encode-dll`).
2. **Decode the 200 response**: rebuild the probe and run it — `fmt_plist` will print the response
   (expect `streams` with `dataPort`, possibly `eventPort`/`timingPort`). This tells you what the receiver
   expects next and confirms a PTP session is fully accepted.
3. **Make `timingProtocol` dynamic** in `crates/rotten-protocol/src/mirror_rtsp.rs`:
   - Add helpers to `rotten-core/src/device.rs` `DeviceFeatures`, e.g. `supports_ntp_clock()` (bit 45)
     and `supports_ptp_clock()` (bit 41).
   - In `encode_audio_setup_plist_chacha` and `encode_video_setup_plist`, emit `"PTP"` when the receiver
     has PTP but not NTP, otherwise `"NTP"` (doubletake compatibility for Apple TV).
4. **Video SETUP + RECORD + data channel**: with PTP accepted, continue the existing flow
   (`video SETUP` with `shk`/`shiv`, `RECORD`, connect to `dataPort`, send test frames via
   `--test`). Verify with `--test` first, then real screen capture.
5. **PTP timing**: the receiver is a PTP clock (OpenAVNU). Determine whether the sender must sync to the
   receiver's PTP clock (UDP multicast 224.0.1.129:319/320) or whether frame timestamps relative to the
   sender's own epoch are accepted initially. The `timingPort` field may be ignored in PTP mode; watch the
   response plist and `POST /feedback` for hints. Adjust `rotten-protocol/src/ntp.rs` usage accordingly.
6. **Validation checklist**: `--test` stream visible on the projector → RECORD 200 → frames visible →
   feedback/heartbeat stays alive → audio (optional) → real capture.
7. **Potential fallback if PTP proves mandatory and hard**: implement minimal PTP master/slave using the
   `PTPInfo` OpenAVNU behavior, or check whether the receiver accepts `timingProtocol: "None"` for
   mirroring (unverified).

---

## 8. Useful references used

- doubletake (AirPlay sender, Go) — skip fp-setup for non-FairPlay receivers:
  https://github.com/alexjsteffen/doubletake/commit/0ddfeea579b3de738e246020a1270452e6fe3abb
- UxPlay (AirPlay 2 receiver, C) — SETUP handler and NTP assumptions:
  https://github.com/FDH2/UxPlay (`lib/raop_handlers.h`, `lib/raop_rtp_mirror.c`)
- airplay2-receiver (protocol notes + feature flag names):
  https://github.com/openairplay/airplay2-receiver (`ap2/bitflags.py`)
- FairPlay SAP research (404 = no FairPlay, 403 = not paired):
  https://github.com/objevovat/fairplay-sap-core-airplay2-sender-authentication-handshake
- Unofficial AirPlay spec (legacy mirroring HTTP flow):
  https://openairplay.github.io/airplay-spec/screen_mirroring/http_requests.html

---

## 9. Quick cheat sheet

```
Projector:            192.168.137.247, AirPlay port 47439 (dynamic; re-probe /info after toggling AirPlay)
Pairing code:         1234 (custom, set on projector)
Creds file:           %APPDATA%\cermin\credentials.json
Debug trace:          %TEMP%\cermin-debug.log  (set CERMIN_DEBUG_LOG=1)
Credentials:          device_id BC:7E:8B:76:0E:22, identifier 8e70f5df22738290, hap=true
Feature bits:         0x038bcb46007f8ad0 — bit 7 mirroring, bit 14 FairPlay OFF, bit 38 HAP encrypt,
                      bit 41 PTP ON, bit 45 NTP OFF, bit 46/48 HAP/transient pairing
Working SETUP:        timingProtocol = "PTP"  → HTTP 200 + 397-byte plist (response not yet decoded)
Failing SETUP:        timingProtocol = "NTP"  → server silent (no bytes, forever)
Endpoint map:         OPTIONS 200, POST /feedback 200, GET /info 200, POST /stream 404,
                      /auth-setup 400, /audioMode 404, /fp-setup 404
Interrupted session:  opencode session ses_f4bf6c564ffeLrcAlKCs1Iovrg
```
