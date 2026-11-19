# spacedesk-viewer — client natif Linux (Ubuntu 24.04)

Client natif haute performance pour [spacedesk](https://spacedesk.net/), réécrit à
partir de zéro en **Rust** (l'app Electron du dépôt consomme ~200 Mo et fait du
décodage logiciel dans Chromium ; ici le décodage H264 est fait en **matériel via
VA-API** et l'affichage en **OpenGL**, avec conversion YUV→RGB sur le GPU).

Le protocole binaire spacedesk (v4.8, WebSocket sur le port **28252**) a été
extrait du viewer HTML5 officiel embarqué dans ce dépôt (`src/spacedesk.min.js`) :
handshake `Identification`, trames `FrameBuffer` (H264 / JPEG / raw, éventuellement
fragmentées), acquittements `FlowControlAck`, curseur, visibilité, et envoi
clavier/souris/touch `KVM`.

## Architecture

```
native/
├── core/    bibliothèque 100 % std Rust (aucune dépendance externe)
│   ├── protocol.rs   paquets binaires (layouts exacts du viewer officiel)
│   ├── ws.rs         client WebSocket RFC 6455 (std uniquement)
│   ├── session.rs    connexion, handshake, réassemblage des fragments, ACK, KVM
│   ├── decoder.rs    trait Decoder + décodeur raw (non compressé)
│   └── ffmpeg.rs     décodeur H264/JPEG via pipe `ffmpeg` (VA-API hw accel)
├── cli/     binaire headless : stats, dump de frames (PPM) — testable sans écran
├── gui/     application fenêtrée : winit + glutin + OpenGL (NV12/BGRA → GPU)
└── examples/mock_server.py   faux serveur spacedesk pour tester sans Windows
```

- **Performance** : priorité au H264 (`compression=1`, `clientType=WindowsRemoteMonitor`),
  décodage VA-API (`-hwaccel vaapi -hwaccel_output_format nv12`) quand un GPU
  Intel/AMD est présent, upload d'une texture NV12 (2 plans) et conversion
  YUV→RGB dans le shader. Repli automatique : JPEG (`--compression jpeg`),
  puis raw (`--compression off`). Le décodage passe par le binaire `ffmpeg`
  (pas de bindings FFI fragiles) : installez le paquet `ffmpeg`.
- **Entrées** : souris envoyée en deltas relatifs (comme le viewer HTML5) avec
  `set_cursor_grab`, molette, boutons ; clavier via table VK Windows
  (`F11` = plein écran, `Échap` = libère la souris).

## Prérequis (Ubuntu 24.04)

```bash
sudo apt install build-essential ffmpeg mesa-va-drivers \
    libxkbcommon-dev libwayland-dev libegl1 libgl1 libx11-6
```

(`mesa-va-drivers` = décodage VA-API Intel/AMD ; pour NVIDIA, utilisez le pilote
propriétaire et éventuellement `--hwaccel none`.)

## Compilation

```bash
cd native/core && cargo test                 # tests du protocole/transport (hors ligne OK)
cd native/cli && cargo build --release       # binaire headless : spacedesk-cli
cd native/gui && cargo build --release       # binaire GUI    : spacedesk-viewer
```

> `core` et `cli` n'ont **aucune dépendance externe** (std uniquement) et se
> compilent/testent même sans accès réseau. `gui` a besoin de crates.io
> (`winit`, `glutin`, `glow`) pour se compiler.

## Utilisation

```bash
# GUI (écran distant + contrôle clavier/souris)
native/gui/target/release/spacedesk-viewer 192.168.1.10
native/gui/target/release/spacedesk-viewer 192.168.1.10 --fps 60 --quality 80 --fullscreen

# Headless : statistiques + dump des frames décodées (pour valider sans écran)
native/cli/target/release/spacedesk-cli 192.168.1.10 --compression h264 \
    --frames 300 --dump /tmp/frames

# Test complet sans serveur Windows (faux serveur spacedesk en Python)
python3 native/examples/mock_server.py --port 28252 --frames 60
native/cli/target/release/spacedesk-cli 127.0.0.1 --compression off \
    --frames 60 --dump /tmp/frames --no-reconnect
```

Options communes : `--port`, `--compression h264|jpeg|off`, `--quality 1-100`,
`--subsampling 0|1|2`, `--fps 30|60`, `--resolution WxH|client`,
`--hwaccel auto|vaapi|none`, `--no-reconnect` (voir `--help`).

## Protocole (référence rapide)

- WebSocket binaire vers `ws://<serveur>:28252`, un paquet par message :
  header 128 octets (LE) + payload (`header[4..8]` = taille du payload).
- `Identification` (client→serveur, 128+334 octets) : version 4.8, clientType,
  compression, qualité, sous-échantillonnage, fps, résolutions (offsets 8..92).
- `FrameBuffer` (serveur→client) : width@8, height@12, pitch@16, format@20,
  rect@24..36, compression@40, fragmentInfo@64 ; payload = H264/JPEG/raw.
- `FlowControlAck` (type 7, header 128 octets) envoyé après chaque trame complète
  (ou tous les 4 fragments, comme le viewer HTML5).
- KVM : `Mouse` (10), `Keyboard` (11), `Touch` (12), `Disconnect` (8, 28 octets).

## Limites connues / pistes d'amélioration

- Le décodage passe par un pipe vers `ffmpeg` (lu en NV12 empaqueté) : très
  rapide jusqu'en 1440p60 ; le **zero-copy VA-API → EGL/DMABUF** est la
  prochaine étape pour le 4K60.
- Le protocole a été validé byte-exact contre le viewer officiel et testé de
  bout en bout avec `examples/mock_server.py` ; testez contre votre serveur
  Windows réel (`spacedesk-cli ... --frames 50` d'abord).
