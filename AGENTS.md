# AGENTS.md

## Project overview

Soundwave streams the Windows default output device (WASAPI loopback) as PCM to one Android phone over LAN QUIC. V0.1 scope: no cloud, no accounts, no codec (Opus is planned later), no mDNS, one client at a time. After a drop, the client reconnects automatically with exponential backoff until the server returns or the user disconnects. See README.md for the full design rationale.

Data flow: WASAPI loopback -> shared-mode conversion -> f32 -> i16 PCM -> bounded 10 ms capture queue -> QUIC datagrams; Android client -> jitter buffer -> SPSC PCM ring -> Kotlin AudioTrack.

Ownership split: Rust owns QUIC, packet validation, certificate pinning, jitter buffer, and PCM ring. Kotlin owns Android lifecycle, notification, Wi-Fi lock, and AudioTrack. Kotlin must not own streaming objects or network state.

## Repository layout

```text
crates/protocol/        # Manual datagram encoding (fixed 12-byte header) + postcard control messages
crates/audio-common/    # PCM format helpers, SPSC ring (rtrb), conversion utilities
crates/transport/       # Quinn helpers, self-signed cert generation, fingerprint-pinned TLS
windows-server/         # WASAPI loopback CLI (clap) + QUIC sender; cfg(windows)-gated
android/app/src/main/rust/     # Android QUIC/jitter/ring JNI cdylib ("soundwave_native")
android/app/src/main/java/com/example/audiostream/  # MainActivity, AudioService, NativeBridge, NotificationHelper
```

## Stack

- Rust, edition 2024, rust-version 1.85; toolchain pinned via `rust-toolchain.toml` (stable + clippy + rustfmt).
- tokio, quinn 0.11 (rustls 0.23 with ring), postcard, serde, rtrb (SPSC ring), crossbeam-channel, thiserror, tracing; `jni` + `arc-swap` + `crossbeam-queue` on the Android crate; `wasapi` is a `[target.'cfg(windows)'.dependencies]`-only dep.
- All dependencies are declared in the root `Cargo.toml` `[workspace.dependencies]` and referenced as `x.workspace = true`; internal crates use path deps. Add new deps there, not in crate manifests.
- Android: Kotlin 2.0.21, AGP 8.7.3, compileSdk/targetSdk 36, minSdk 26, Java 17, ABIs arm64-v8a + x86_64.

## Commands (verified working on Linux host)

```bash
cargo check --workspace --all-targets          # also passes with --all-features
cargo test --workspace                          # 14 tests pass
cargo fmt --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo lint
```

Windows server binary: `cargo build --release -p audio-stream-server`. On non-Windows hosts the server crate still compiles but its WASAPI modules are `cfg(windows)`-gated and `main` prints a notice; its tests do not run on Linux. The Android native crate compiles on the host and its tests do run.

Android packaging is NOT done via cargo: Gradle task `buildRustNative` runs `cargo ndk -t arm64-v8a -t x86_64 -o ../jniLibs build --release` from `android/app/src/main/rust`, producing `libsoundwave_native.so`. Requires NDK, `cargo-ndk`, and the `aarch64-linux-android`/`x86_64-linux-android` targets. No CI exists.

## Coding conventions

- Rust 2024 idioms: `#![deny(unsafe_op_in_unsafe_fn)]` at crate top (server main.rs and Android lib.rs); `#[unsafe(no_mangle)]` and `pub extern "system"` for JNI exports.
- JNI exports are named `Java_com_example_audiostream_NativeBridge_native*` and mirror `NativeBridge.kt`; renaming one side breaks the other.
- Unit tests live inline as `#[cfg(test)] mod tests`; no integration test dirs. Doc comments (`///`) on public items are the norm.
- No emoji anywhere (source, comments, docs, commit messages).

## Architecture rules to preserve

- Wire format: fixed 12-byte network-order header (`sequence u32 | timestamp u64`) + i16 LE PCM; control messages (Hello/StreamInfo/Start/Stop/Ping/Pong) are postcard on a reliable QUIC stream. `PROTOCOL_VERSION` lives in the protocol crate.
- Bounded latency policy: if the capture queue or PCM ring is full, drop the oldest; underrun becomes silence. Never let buffering grow unbounded.
- Datagram sizing: the server uses Quinn's advertised max datagram size and splits 10 ms capture blocks into 5/4/2/1 ms packets as needed; never assume an Ethernet MTU or rely on IP fragmentation.
- Security: server generates a self-signed cert+key on first run in `%LOCALAPPDATA%\Soundwave` (or `--identity-dir`); the client pins the leaf SHA-256 fingerprint. A changed fingerprint is a pairing change, not a certificate error to paper over.
- Jitter target is 50 ms; confirmed-missing sequences become silence, never infinite waits.

## Constraints and pitfalls

- Server supports one streaming client at a time; Windows Firewall must allow inbound UDP 48400 (QUIC is UDP, not TCP).
- Server binary only runs on Windows (WASAPI); it is a compile-time stub elsewhere.
- `--capture-to` writes raw PCM (no WAV header): 48 kHz, stereo, i16 LE.
- `.gitignore` excludes `target/`, `android/app/src/main/jniLibs/`, Gradle artifacts, `*.pcm`, `*.wav` — do not commit generated native libs.
- The Android app needs a physical phone on the same LAN; AP-isolated guest Wi-Fi will not work.
