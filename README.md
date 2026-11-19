# Spacedesk-Viewer

Offline [spacedesk](https://spacedesk.net/) viewer — with a **native Linux client** (Rust) as the main, maintained component.

## 🐧 Native Linux client (`native/`, Rust) — recommended

Raw-TCP / WebSocket spacedesk protocol client with **VA-API hardware H264 decoding** (FFmpeg) and **OpenGL rendering**, plus a headless CLI (`spacedesk-cli`). The GUI (`spacedesk-viewer`) opens a **home menu** (server IP + Connect) with a **settings panel** (transport, compression, quality, FPS, resolution presets + custom, HW accel, KVM…) persisted to `~/.config/spacedesk-viewer/settings.json`.

```bash
# Ubuntu 24.04 dependencies
sudo apt install build-essential ffmpeg mesa-va-drivers \
    libxkbcommon-dev libwayland-dev libegl1 libgl1 libx11-6

git clone https://github.com/0rmx0/Spacedesk-Viewer.git
cd Spacedesk-Viewer/native
cargo build --release

./target/release/spacedesk-viewer                          # home menu
./target/release/spacedesk-viewer 192.168.1.10 --fps 60     # prefill + connect
./target/release/spacedesk-cli 192.168.1.10 --frames 50 -v  # headless test
```

Full documentation (architecture, protocol, options, mock server, roadmap): [`native/README.md`](native/README.md).

Requirements: a **Windows PC with the [spacedesk server](https://spacedesk.net/#downloads) installed** (driver), and any Linux machine as the viewer.

## 🖥️ Legacy offline HTML5 viewer (Electron app in `src/`) — no longer maintained

Adaptation of [spacedesk-HTML5-VIEWER](http://spacedesk.ph/html5viewer/) v0.9.9z.63 for offline use:

+ Remove google-analytics part
+ Localize all resources and rearrange them
+ Beautify the interface
+ Fix a bug with the fullscreen feature

```bash
npm install
npm run start
```

> [!NOTE]
> The Electron app above is kept for reference only — its dependencies are very outdated. On Linux, use the **native client** instead.

> Resources from [Liumeo](https://github.com/eagleoflqj "Liumeo's github page"), big thanks to him!
