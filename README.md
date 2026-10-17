# Screen Sharing


## Features

- **Direct Peer-to-Peer Streaming:** Video (H.264) and audio (Opus) flow directly between peers over WebRTC without intermediate media servers.
- **Application-Isolated Audio Capture:** Captures audio from a specific application/process tree (Windows WASAPI Process Loopback and Linux PipeWire) or full system audio, preventing voice chat echo.
- **On-Demand Stream Playback:** Joining a room uses minimal CPU and memory. Video and audio decoders are initialized only when you choose to watch a participant's live stream.
- **Configurable Quality:** Supports 720p, 1080p, or native resolution at 15, 30, or 60 FPS.
- **Unlimited Room Participants:** Any number of peers can join a room, watch active streams, or share their own screen.

## Prerequisites

### Linux (Ubuntu / Debian)

```bash
sudo apt update && sudo apt install -y \
  curl build-essential cmake pkg-config libclang-dev \
  libwebkit2gtk-4.1-dev libgtk-3-dev libayatana-appindicator3-dev \
  librsvg2-dev libpipewire-0.3-dev libspa-0.2-dev libasound2-dev \
  libxcb1-dev libxrandr-dev libdbus-1-dev

curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
source "$HOME/.cargo/env"
rustup target add wasm32-unknown-unknown
```

### Windows 10 (Build 2004+) / Windows 11

Run in PowerShell:

```powershell
winget install Rustlang.Rustup
winget install Kitware.CMake
winget install NASM.NASM
winget install Microsoft.VisualStudio.2022.BuildTools --override "--wait --passive --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended"
```

Restart PowerShell and add the WebAssembly target:

```powershell
rustup target add wasm32-unknown-unknown
```

## Building

Always build with `--release` so real-time video encoding runs with full compiler optimizations:

```bash
# 1. Build the WebAssembly frontend
cargo build -p client-ui --target wasm32-unknown-unknown --release

# 2. Build the signaling server and desktop client
cargo build --release -p signaling-server -p desktop-client
```

The compiled binaries will be placed in `target/release/desktop-client` (`desktop-client.exe` on Windows) and `target/release/signaling-server`.

## Usage

1. **Start the Signaling Server:**
   ```bash
   cargo run --release -p signaling-server
   ```
   By default, the server listens on `ws://0.0.0.0:9001/ws` (configurable via `SIGNALING_BIND_ADDR` or `PORT`).

2. **Launch the Desktop Client:**
   ```bash
   cargo run --release -p desktop-client
   ```

3. **Create or Join a Room:**
   - Enter the signaling server URL (e.g., `ws://127.0.0.1:9001/ws` for local testing, `ws://<LAN_IP>:9001/ws` on a local network, or `wss://<DOMAIN>/ws` over the internet).
   - Click **Create Room** to start a new room and share the generated **Room ID**, or paste an existing **Room ID** and click **Join Room**.
   - Click **Watch Stream** next to any live participant to view their stream, or click **Share Screen** to broadcast your own display or application window.

## Deploying the Signaling Server

Because media flows directly peer-to-peer, the signaling server only exchanges lightweight handshake messages and can be hosted on free container platforms:

- **Docker (Render, Koyeb, Fly.io):** Deploy the repository using the included `Dockerfile` (or `render.yaml`) and connect clients using `wss://<your-app-domain>/ws`.
- **Cloudflare Quick Tunnel:** Expose a locally running signaling server without deploying to the cloud:
  ```bash
  cloudflared tunnel --url http://localhost:9001
  ```
  Then connect clients using `wss://<generated-subdomain>.trycloudflare.com/ws`.

## Testing

```bash
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

## License

Licensed under the [GNU General Public License v3.0](./LICENSE) (GPL-3.0).
