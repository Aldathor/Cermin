# Third-Party Notices

Cermin (MIT OR Apache-2.0) incorporates or invokes the following third-party components.

## OpenH264 (Cisco)

- **Use:** Software H.264 encoding via the `openh264` Rust crate.
- **License:** BSD-style (see [OpenH264](https://www.openh264.org/)).
- **Windows:** `openh264-2.6.0-win64.dll` is downloaded by `scripts/build-windows.sh` from Cisco's binary distribution and must ship alongside `cermin.exe`.

## Playfair (C)

- **Use:** FairPlay SAP step 1 encryption, statically compiled into `rotten-crypto`.
- **Location:** `crates/rotten-crypto/vendor/playfair/`
- **Origin:** Derived from reverse-engineered AirPlay FairPlay code (UxPlay / community implementations).
- **License:** Treat as GPL-2.0-or-later derived work. Source is included in-tree.

## fpsap-helper (Go)

- **Use:** FairPlay SAP step 2 hash (`fp-setup` m2). Invoked **only as a separate subprocess**, never linked into the MIT binary.
- **Location:** `tools/fpsap-helper/`
- **License:** GPL-3.0 (see `tools/fpsap-helper/LICENSE`).
- **Distribution:** Build with Go (`scripts/build-windows.sh`) and ship `fpsap-helper` / `fpsap-helper.exe` next to `cermin`. Do not embed the GPL binary inside `cermin`.

## doubletake (reference)

- **Use:** SAP hash algorithm reference for `fpsap-helper`.
- **License:** GPL-3.0 — https://github.com/omarroth/doubletake

## Other Rust dependencies

See `Cargo.lock` for the full dependency graph. Notable crypto crates: `p256`, `x25519-dalek`, `ed25519-dalek`, `chacha20poly1305`, `aes-gcm`.
