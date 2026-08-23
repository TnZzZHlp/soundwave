# Soundwave build and verification tasks.
# Install Just with: cargo install just

# Build the release Windows server and Android APK by default.
default: build-server build-android

# Build all Rust workspace crates and the Android release APK.
build: build-workspace build-android

# Build all Rust workspace crates for the current host.
build-workspace:
    cargo build --release --workspace

# Build the Windows server only.
build-server:
    cargo build --release -p audio-stream-server

# Build the signed Android release APK and its native Rust libraries.
[windows]
build-android:
    cd android && .\gradlew.bat :app:assembleRelease

[unix]
build-android:
    cd android && ./gradlew :app:assembleRelease

# Check all Rust workspace targets and features.
check:
    cargo check --workspace --all-targets --all-features

# Run all Rust workspace tests.
test:
    cargo test --workspace --all-targets

# Check Rust formatting.
format-check:
    cargo fmt --check

# Run all Rust linters.
lint:
    cargo clippy --workspace --all-targets --all-features -- -D warnings
    cargo lint

# Run all Rust validation tasks.
verify: format-check check test lint
