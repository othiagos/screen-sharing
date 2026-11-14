# Screen Sharing

## Highlights

- **Centralized QUIC SFU (`sfu-server` via `quinn`):** Dedicated QUIC unidirectional streams per H.264 video frame eliminate TCP head-of-line blocking, while unreliable QUIC datagrams (with automatic stream fallback) deliver low-latency 20ms Opus audio.
- **Native Hardware UI (`gpui-kit`):** Zero WebView/WebAssembly overhead — dark Discord/Parsec-style native window with direct GPU `RenderImage` BGRA video presentation, per-peer 0–200% audio mixer sliders, Grid/Focus/Fullscreen views, live preview Capture Picker, and real-time latency/bitrate stats (`Ctrl+Shift+S`).
- **Stutter-Free Game Capture:**
  - **Windows 10/11:** `Windows.Graphics.Capture` (`CreateFreeThreaded` + HWND validation to eliminate Alt-Tab desktop leaks), DXGI Desktop Duplication with idle frame deduplication, **Media Foundation Hardware H.264 MFT** (NVENC/AMF/QuickSync when available, `OpenH264` tuned fallback), and WASAPI Process Loopback.
  - **Linux (Wayland & X11):** XDG Desktop Portal ScreenCast (`ashpd`) + PipeWire video streaming on Wayland, `xcap` on X11, bilinear SIMD scaling (`fast_image_resize`), and PipeWire per-application audio capture via `target.object` (`object.serial`).
- **TLS 1.3 + TOFU Security:** Self-signed servers automatically display a **Trust-On-First-Use (TOFU)** SHA-256 fingerprint prompt and warn immediately if the pin changes. Supports `SFU_TLS_CERT`/`SFU_TLS_KEY` PEM files, optional `SFU_AUTH_TOKEN`, and per-room passwords.
- **Audio Resilience:** 40ms per-peer jitter buffer with Opus Packet Loss Concealment (PLC), automatic drift trimming, and soft-clipped mixing.

## Prerequisites

### Linux (Ubuntu / Debian)

```bash
sudo apt update && sudo apt install -y \
  curl build-essential cmake pkg-config libclang-dev nasm \
  libpipewire-0.3-dev libspa-0.2-dev libasound2-dev \
  libxcb1-dev libxrandr-dev libdbus-1-dev \
  libxkbcommon-dev libxkbcommon-x11-dev libwayland-dev libvulkan-dev \
  libfontconfig1-dev libfreetype-dev

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

Always compile with `--release` so SIMD scaling and H.264/Opus encoding run at full speed:

```bash
cargo build --release -p sfu-server -p desktop-client
```

Binaries are written to `target/release/sfu-server` and `target/release/desktop-client` (`.exe` on Windows).

## Running

1. **Start the QUIC SFU Server** (default bind `0.0.0.0:4433` UDP):
   ```bash
   SFU_AUTH_TOKEN="optional-secret" cargo run --release -p sfu-server
   ```
   Optional environment variables:
   - `SFU_BIND_ADDR` / `SFU_BIND`: bind address (default `0.0.0.0:4433`)
   - `SFU_AUTH_TOKEN`: optional shared access token required on `Hello`
   - `SFU_CERT_DIR`: persistent directory for auto-generated self-signed TLS certs (default `./.sfu-certs`)
   - `SFU_PUBLIC_HOST`: comma-separated SAN DNS/IP entries when auto-generating certs
   - `SFU_TLS_CERT` & `SFU_TLS_KEY`: explicit PEM cert/key paths (e.g., Let's Encrypt)

2. **Launch the Desktop Client**:
   ```bash
   cargo run --release -p desktop-client
   ```
   - Enter `server:4433`, your nickname, and optional `auth_token`.
   - Accept the server's SHA-256 TOFU fingerprint prompt on first connection.
   - Click **Criar Nova Sala** or enter an existing Room ID and click **Entrar na Sala**.
   - Click **Compartilhar Tela** to pick a Monitor, Window, or Wayland Portal, preview it live, select `720p`/`1080p`/`1440p`/`Nativa`, `15`/`30`/`60 FPS`, bitrate (`6`/`12`/`20 Mbps`/`Auto`), and audio mode.

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
