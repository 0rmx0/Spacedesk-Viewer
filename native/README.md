# spacedesk-viewer — client natif Linux (Ubuntu 24.04)

Client natif haute performance pour [spacedesk](https://spacedesk.net/), réécrit à
partir de zéro en **Rust** (l'app Electron du dépôt consomme ~200 Mo et fait du
décodage logiciel dans Chromium ; ici le décodage H264 est fait en **matériel via
VA-API** et l'affichage en **OpenGL**, avec conversion YUV→RGB sur le GPU).

Le protocole binaire spacedesk (v4.8, port **28252**) a été extrait du viewer
HTML5 officiel embarqué dans ce dépôt (`src/spacedesk.min.js`) **et** confirmé
contre des clients natifs reverse-engineered qui parlent au driver en **TCP
brut** (le port 28252 sniffe les premiers octets : `GET ...` -> mode WebSocket
du viewer HTML5, sinon paquets binaires bruts natifs). Notre client utilise le
**TCP brut par défaut** (`--transport ws` pour forcer le mode WebSocket) avec
le handshake natif (clientType=WindowsRemoteMonitor, GUID UTF-16LE dans le
payload, fps réel à l'offset 44, flags 0x10003 à l'offset 36). Les trames
`FrameBuffer` arrivent en **tuiles** (rect de destination + JPEG/H264/raw,
éventuellement fragmentées x4) : elles sont compositées sur le GPU (FBO +
quad texturé). Les `Ping` du serveur sont ré-échés (keep-alive), les
acquittements `FlowControlAck` sont envoyés après chaque trame complète (ou
tous les 4 fragments), et le clavier/souris/touch `KVM` est renvoyé.

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

- **Performance** : priorité au H264 (`compression=1`), décodage VA-API
  (`-hwaccel vaapi`) quand un GPU Intel/AMD est présent, sortie **BGRA** de
  ffmpeg uploadée telle quelle en texture OpenGL (zéro conversion CPU côté
  client), compositing des tuiles sur GPU. Repli automatique d'identification :
  TCP+H264 -> TCP+JPEG -> WS+JPEG (combo du viewer HTML5) -> WS+raw.
  Le décodage passe par le binaire `ffmpeg` (pas de bindings FFI fragiles) :
  installez le paquet `ffmpeg`.
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
> compilent/testent même sans accès réseau. `gui` utilise `winit 0.29` +
> `glutin 0.31`/`glutin-winit 0.4` + `glow 0.14` (via crates.io) et a été
> compilé avec succès en release sur Ubuntu 24.04.

## Utilisation

```bash
# GUI (écran distant + contrôle clavier/souris)
native/gui/target/release/spacedesk-viewer 192.168.1.10
native/gui/target/release/spacedesk-viewer 192.168.1.10 --fps 60 --quality 80 --fullscreen

# Headless : statistiques + dump des frames décodées (pour valider sans écran)
native/cli/target/release/spacedesk-cli 192.168.1.10 --compression h264 \
    --frames 300 --dump /tmp/frames

# Test complet sans serveur Windows (faux serveur spacedesk en Python)
python3 native/examples/mock_server.py --port 28252 --frames 60 --tcp   # mode natif TCP
native/cli/target/release/spacedesk-cli 127.0.0.1 --compression off \
    --frames 60 --dump /tmp/frames --no-reconnect -v
```

Options communes : `--port`, `--transport tcp|ws`, `--compression h264|jpeg|off`,
`--client-type 0|1|3`, `--quality 1-100`, `--subsampling 0|1|2`, `--fps 30|60`,
`--resolution WxH|client`, `--hwaccel auto|vaapi|none`, `--no-reconnect`,
`-v/--verbose` (voir `--help`).

## Protocole (référence rapide)

- TCP brut vers `<serveur>:28252` (mode natif, par défaut) ou WebSocket
  binaire `ws://<serveur>:28252` (mode viewer HTML5, `--transport ws`).
  Un paquet = header 128 octets (LE) + payload (`header[4..8]` = taille).
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
