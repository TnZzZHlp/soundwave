# Soundwave — Windows to Android LAN audio

Soundwave captures the current Windows output device with WASAPI loopback and streams PCM to one Android phone over LAN QUIC. It is intentionally a small V0.1: no cloud service, accounts, WebRTC, RTP, virtual audio device, or codec is involved.

```text
Windows system audio
  WASAPI loopback → Windows shared-mode conversion → f32 → i16 PCM
  bounded 10 ms capture queue → QUIC Datagram
  reliable QUIC control stream (Hello / StreamInfo / Start / Stop / Ping / Pong)
  Android QUIC client → jitter buffer → lock-free PCM ring
  Kotlin AudioTrack platform shim → speaker / headset
```

The Android foreground service owns the process lifecycle and notification. Rust owns the QUIC connection, packet parsing, certificate pinning, jitter buffer, and PCM ring; Kotlin does not own streaming objects or network state.

The Android native crate is included in the top-level Cargo workspace so its portable protocol, jitter, and JNI code receives the same host-side checks and tests. Android ABI packaging still happens separately through `cargo-ndk`.

## Project layout

```text
.
├── crates/
│   ├── protocol/       # Manual audio datagram encoding and postcard control messages
│   ├── audio-common/   # PCM format helpers and tested SPSC ring utility
│   └── transport/      # Quinn control/datagram helpers and fingerprint-pinned TLS
├── windows-server/     # WASAPI loopback CLI and QUIC sender
└── android/
    └── app/
        └── src/main/
            ├── java/com/example/audiostream/ # Activity, foreground service, notification, JNI declarations
            └── rust/                         # Android QUIC/jitter/ring JNI cdylib
```

## Wire format and latency policy

Every audio datagram has a fixed manual, network-byte-order header:

```text
0              4              12
+--------------+--------------+----------------------
| sequence u32 | timestamp u64| PCM i16 LE payload
+--------------+--------------+----------------------
```

The normal source format is 48 kHz, stereo, signed 16-bit little-endian PCM. Capture produces 10 ms blocks (480 samples per channel, 1,920 bytes). Before sending, the server checks Quinn's advertised maximum datagram size. It uses 10 ms when safe; otherwise it splits a capture block into 5 ms, 4 ms, 2 ms, or 1 ms packets and advertises that frame duration in `StreamInfo`. It never assumes a fixed Ethernet MTU or relies on IP fragmentation.

The live path is deliberately bounded. If the capture queue or PCM ring is full, the oldest audio is discarded. A playback underrun becomes silence. This prevents unbounded latency growth when Wi-Fi or the output device falls behind.

The Android jitter target is 50 ms. It tolerates small reordering, converts a confirmed missing sequence to silence, and never waits forever for a lost packet.

## Security and pairing

QUIC always uses TLS. On its first launch, the Windows server creates a self-signed certificate and private key in:

```text
%LOCALAPPDATA%\Soundwave
```

It prints the SHA-256 certificate fingerprint. Copy that fingerprint into the Android app with the server IP and port. Android pins the actual leaf certificate hash; it does not accept arbitrary certificates. The identity persists across server restarts, so the fingerprint stays stable unless that directory is removed or `--identity-dir` is changed.

Treat a changed fingerprint as a pairing change. Do not accept a fingerprint from an untrusted source.

## Requirements

Windows server:

- Windows 10 or Windows 11
- Rust stable with the MSVC toolchain
- An active default Windows render/output device

Android app:

- Android Studio with Android SDK and Android NDK installed
- Android SDK Platform 36 and Build Tools 36.0.0
- JDK 17
- Rust stable
- `cargo-ndk`
- A physical `arm64-v8a` phone is the primary target; `x86_64` is included for an emulator

Both devices must be on the same LAN. Enterprise/guest Wi-Fi networks with AP isolation will usually prevent the connection.

## Build the Windows server

From the repository root:

```powershell
cargo build --release -p audio-stream-server
```

The executable is `target\release\audio-stream-server.exe`.

Useful development checks:

```powershell
cargo fmt --check
cargo test --workspace --all-targets
cargo check --workspace --all-targets --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

### Validate capture before networking

This writes raw 48 kHz, stereo, i16 little-endian PCM and is useful for verifying that WASAPI loopback sees the system mix:

```powershell
cargo run -p audio-stream-server -- --capture-to capture.pcm --capture-seconds 10
```

The result is raw PCM, not a WAV container. Open it in an audio editor as `48000 Hz`, `signed 16-bit little-endian`, `stereo`.

## Run the Windows server

```powershell
$env:RUST_LOG = "info"
target\release\audio-stream-server.exe
```

By default it listens at `0.0.0.0:48400`. To choose another bind address or identity directory:

```powershell
audio-stream-server --bind 0.0.0.0:48400 --identity-dir C:\SoundwaveIdentity
```

The startup output shows the default playback device, its mix format, the fixed capture format, server address, and pairing fingerprint. The server supports one streaming client at a time in V0.1. Press `Ctrl+C` to stop capture, close the connection, join workers, and release WASAPI.

## Build the Android app

1. Install the NDK through Android Studio and point `ANDROID_NDK_HOME` (or `ANDROID_NDK_ROOT`) at it.
2. Install `cargo-ndk` and Android Rust targets:

   ```powershell
   cargo install cargo-ndk
   rustup target add aarch64-linux-android x86_64-linux-android
   ```

3. Open `android/` in Android Studio, let Gradle sync, and build `app`. The `buildRustNative` Gradle task runs:

   ```powershell
   cd android\app\src\main\rust
   cargo ndk -t arm64-v8a -t x86_64 -o ../jniLibs build --release
   ```

   It packages `libsoundwave_native.so` for the selected ABIs before the APK is assembled.

4. Install the debug APK on the phone.

The Gradle configuration uses API 36, min SDK 26, and Java 17. If you use a command-line Gradle installation instead of Android Studio, run this from `android/`:

```powershell
gradle :app:assembleDebug
```

## Use the Android app

1. Start the Windows server and copy its fingerprint.
2. In **Audio Stream**, enter the Windows LAN IP, port `48400`, and fingerprint.
3. Tap **Connect**.
4. Allow notification permission when Android asks.

The app starts a `mediaPlayback` foreground service before connecting and holds a Wi-Fi lock only while a live stream is active. The notification has a **Disconnect** action. Audio continues when the activity is destroyed, the app is switched away from, the screen is off, or the phone is locked. Disconnect stops the QUIC task, clears the ring, stops AudioTrack, releases the Wi-Fi lock, removes the foreground notification, and stops the service.

The UI shows a conservative estimated latency, current buffer duration, and packet/loss/late/underrun counters. The latency number is an estimate (`RTT / 2 + jitter/ring buffering`), not a claim of exact speaker-output latency.

## Windows Firewall

Allow inbound **UDP 48400** for `audio-stream-server.exe`. QUIC uses UDP; opening TCP 48400 alone is not sufficient.

## Troubleshooting

| Symptom | Check |
| --- | --- |
| Android cannot connect | Verify both devices are on the same subnet, Windows Firewall permits UDP 48400, and guest/AP isolation is disabled. |
| Certificate error | Copy the current server fingerprint exactly. Delete neither the server identity directory nor the app’s saved pairing unintentionally. |
| No sound | Run `--capture-to capture.pcm` and inspect the raw file. Verify Windows is playing through its default output device. |
| Immediate disconnect | Confirm the Android app and server use the same protocol version and that the phone can reach the entered IP address. |
| Repeating underruns or clicks | Use a stronger Wi-Fi signal, disable power-saving restrictions for the app, and avoid congested 2.4 GHz networks. The client fills missing output with silence instead of building delay. |
| High latency | Check the displayed buffer/loss counters. Persistent packet loss or ring overflow means the network/output cannot sustain the stream. |
| Android native build fails | Ensure `cargo-ndk`, the matching NDK, and `aarch64-linux-android` Rust target are installed; then rebuild `buildRustNative`. |

## Current V0.1 boundaries

This project intentionally does not yet provide Opus, mDNS discovery, auto-reconnect, device selection, multiple clients, iOS/Linux/macOS support, public-Internet traversal, or accounts. PCM transport remains codec-neutral at the QUIC packet layer so an Opus encoder/decoder can be introduced later without replacing the control or datagram transport.
