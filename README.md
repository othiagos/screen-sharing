# Screen Share

## Highlights

- **Centralized Low-Latency SFU (`sfu-server` via `quinn`):** Dedicated unidirectional QUIC streams per video frame eliminate TCP head-of-line blocking, while unreliable UDP datagrams (with automatic stream fallback) deliver low-latency 20ms Opus audio.
- **Native Hardware-Accelerated UI (`gpui-kit`):** Zero WebView/Electron overhead — light/dark native interface with direct GPU `RenderImage` BGRA presentation, per-peer 0–200% audio mixer sliders, Grid/Focus/Fullscreen views, live preview Capture Picker, and on-demand technical overlay (**Details** / `Ctrl+Shift+S`).
- **Multilingual (`i18n`):** Built-in support for **English** (default), **Español**, and **Português (BR)** with automatic OS language detection on first launch and instant switching in the Lobby.
- **Sharp Text 4:4:4 Mode & Automatic VBR:**
  - **Sharp Text (`5 FPS`):** Dedicated 64×64 dirty-tile 4:4:4 codec (`SolidRgb` + lossless `QoiRgb` + YUV 4:4:4 JPEG fallback) for crystal-clear code, spreadsheets, and documents without 4:2:0 color bleed.
  - **Smooth Motion (`15` / `30` / `60 FPS`):** Automatic VBR congestion control with SIMD YUV conversion, **Media Foundation Hardware H.264 MFT** (NVENC/AMF/QuickSync on Windows), and multi-slice **OpenH264** fallback.
- **Stutter-Free Screen & Audio Capture:**
  - **Windows 10/11:** `Windows.Graphics.Capture` (`CreateFreeThreaded` + HWND validation), DXGI Desktop Duplication with idle frame deduplication, and WASAPI per-application / system loopback audio.
  - **Linux (Wayland & X11):** XDG Desktop Portal ScreenCast (`ashpd`) + PipeWire video on Wayland, `xcap` on X11, SIMD bilinear scaling (`fast_image_resize`), and PipeWire per-application / system audio capture.
- **TLS 1.3 + TOFU Security:** Self-signed servers prompt for server identity confirmation (**Trust-On-First-Use** SHA-256 verification code) and warn immediately if the identity changes. Supports `SFU_TLS_CERT`/`SFU_TLS_KEY` PEM files, optional `SFU_AUTH_TOKEN`, and per-room passwords.
- **Audio Resilience:** 40ms per-peer jitter buffer with Opus Packet Loss Concealment (PLC), automatic drift trimming, and soft-clipped mixing.

## Prerequisites (Building from Source)

Requires **Rust 1.88+** (Rust Edition 2024).

### Linux (Ubuntu / Debian)

```bash
sudo apt update && sudo apt install -y \
  curl build-essential cmake pkg-config libclang-dev nasm \
  libpipewire-0.3-dev libspa-0.2-dev libasound2-dev \
  libxcb1-dev libxrandr-dev libdbus-1-dev \
  libxkbcommon-dev libxkbcommon-x11-dev libwayland-dev libvulkan-dev \
  libfontconfig1-dev libfreetype-dev libgbm-dev libegl-dev

curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
source "$HOME/.cargo/env"
```

### Windows 10 (Build 2004+) / Windows 11

```powershell
winget install Rustlang.Rustup
winget install Kitware.CMake
winget install NASM.NASM
winget install Microsoft.VisualStudio.2022.BuildTools --override "--wait --passive --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended"
```

## Building

Always compile with `--release` so SIMD scaling and video/audio encoding run at full speed:

```bash
cargo build --release -p sfu-server -p desktop-client
```

Binaries are written to `target/release/sfu-server` and `target/release/desktop-client` (`.exe` on Windows).

## Running

1. **Start the SFU Server** (default bind `0.0.0.0:4433` UDP):
   ```bash
   SFU_AUTH_TOKEN="optional-secret" cargo run --release -p sfu-server
   ```
   Optional environment variables:
   - `SFU_BIND_ADDR` / `SFU_BIND`: bind address (default `0.0.0.0:4433`)
   - `SFU_AUTH_TOKEN`: optional shared access key required on `Hello`
   - `SFU_CERT_DIR`: persistent directory for auto-generated self-signed TLS certs (default `./.sfu-certs`)
   - `SFU_PUBLIC_HOST`: comma-separated SAN DNS/IP entries when auto-generating certs
   - `SFU_TLS_CERT` & `SFU_TLS_KEY`: explicit PEM cert/key paths (e.g., Let's Encrypt)

2. **Launch the Desktop Client**:
   ```bash
   cargo run --release -p desktop-client
   ```
   - Enter the server address (`example.com:4433`), your name, and (if required by the server) the access key under **Advanced options**.
   - Confirm the server identity prompt on first connection.
   - Click **Create New Room** or enter an 8-character room code (e.g. `a1b2c3d4`) and click **Join Room**.
   - Click **Share Screen** to pick a Monitor, Window, or System Screen, preview it live, choose resolution (`720p` / `1080p` / `1440p` / `Original`), mode (`Sharp Text` / `15` / `30` / `60 FPS`), and audio (`No audio` / `Computer audio` / `This app only`).

## Docker Deployment (`sfu-server`)

The repository includes a multi-stage `Dockerfile` for `sfu-server`:

```bash
docker build -t screenshare-sfu .
docker run -d --name sfu \
  -p 4433:4433/udp -p 4433:4433/tcp \
  -e SFU_AUTH_TOKEN="your-secret" \
  -v sfu_certs:/data/certs \
  screenshare-sfu
```

> Make sure UDP port `4433` is open in your cloud firewall / security group — QUIC runs over UDP.

## Verification

```bash
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

## License

Licensed under the [GNU General Public License v3.0](./LICENSE) (GPL-3.0).
