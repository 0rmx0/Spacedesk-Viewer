# spacedesk-viewer — client natif Linux (Ubuntu 24.04)

Client natif haute performance pour [spacedesk](https://spacedesk.net/), réécrit à
partir de zéro en **Rust** (l'app Electron du dépôt consomme ~200 Mo et fait du
décodage logiciel dans Chromium ; ici le décodage H264 est fait en **matériel via
VA-API** et l'affichage en **OpenGL**, avec conversion YUV→RGB sur le GPU).

Le protocole binaire spacedesk (v4.8, port **28252**) a été extrait du viewer
HTML5 officiel embarqué dans ce dépôt (`src/spacedesk.min.js`), **confirmé contre
des clients natifs reverse-engineered indépendants** (voir *Validation externe*
ci-dessous) **et** testé contre un faux serveur (`examples/mock_server.py`). Notre
client utilise le **TCP brut par défaut** (`--transport ws` pour forcer le mode
WebSocket) avec le handshake natif (clientType=WindowsRemoteMonitor, GUID UTF-16LE
dans le payload, fps réel à l'offset 44, flags 0x10003 à l'offset 36). Les trames
`FrameBuffer` arrivent en **tuiles** (rect de destination + JPEG/H264/raw,
éventuellement fragmentées x4) : elles sont compositées sur le GPU (FBO +
quad texturé). Les `Ping` du serveur sont ré-échés (keep-alive), les
acquittements `FlowControlAck` sont envoyés après chaque trame complète (ou
tous les 4 fragments), et le clavier/souris/touch `KVM` est renvoyé.

## Architecture

```
native/                     workspace Cargo (un seul Cargo.lock, un seul target/)
├── Cargo.toml              workspace + profil release (lto)
├── core/    bibliothèque protocole/transport/session/décodeurs
│   ├── protocol.rs   paquets binaires (layouts exacts du viewer officiel)
│   ├── ws.rs         client WebSocket RFC 6455 (std uniquement)
│   ├── transport.rs  TCP brut
│   ├── session.rs    connexion, handshake, réassemblage des fragments, ACK, KVM
│   ├── decoder.rs    trait Decoder + décodeur raw (non compressé)
│   ├── ffmpeg.rs     décodeur H264 via pipe `ffmpeg` (VA-API hw accel, sortie BGRA)
│   └── jpeg.rs       décodeur JPEG en processus (crate jpeg-decoder, sortie BGRA)
├── cli/     binaire headless : stats, dump de frames (PPM) — testable sans écran
├── gui/     application fenêtrée : winit + glutin + OpenGL + UI immediate-mode
│   ├── main.rs       boucle d'événements, menu d'accueil, panneau de réglages
│   ├── gl.rs         renderer OpenGL (tiles GPU + UI 2D + texte)
│   ├── ui.rs         widgets immediate-mode (bouton, champ texte, case, dropdown)
│   ├── font.rs       atlas de glyphes embarqué (ab_glyph, police système)
│   └── vkmap.rs      table VK Windows → codes clavier
├── examples/mock_server.py   faux serveur spacedesk pour tester sans Windows
└── packaging/                build-deb.sh + entrée .desktop
```

- **Performance** : priorité au H264 (`compression=1`), décodage VA-API
  (`-hwaccel vaapi`) quand un GPU Intel/AMD est présent, sortie **BGRA** de
  ffmpeg uploadée telle quelle en texture OpenGL (zéro conversion CPU côté
  client), compositing des tuiles sur GPU. Les tuiles **JPEG** (mode sans ffmpeg
  ou serveur en JPEG) sont décodées **dans le processus** par `jpeg-decoder`.
  Repli automatique d'identification :
  TCP+H264 -> TCP+JPEG -> WS+JPEG (combo du viewer HTML5) -> WS+raw.
  Le décodage H264 passe par le binaire `ffmpeg` (pas de bindings FFI fragiles) :
  installez le paquet `ffmpeg`.
- **Pipeline basse latence** : le décodage tourne sur un **thread worker** dédié ;
  le thread réseau pousse les paquets dans un canal **borné** (64) et le worker
  applique le **latest-wins** sur les tuiles JPEG autonomes (il jette jusqu'à 8
  tuiles anciennes pour ne décoder que les plus fraîches). Résultat : aucun
  backlog ne peut s'accumuler, la latence de bout en bout reste de l'ordre de la
  dizaine de ms même si le décodage est plus lent que le réseau. La file de
  tuiles décodées côté UI est plafonnée (24 tuiles, les plus vieilles partent).
- **Entrées** : souris envoyée en deltas relatifs (comme le viewer HTML5) avec
  `set_cursor_grab`, molette, boutons ; clavier via table VK Windows
  (`F11` = plein écran, `Échap` = retour au menu / libère la souris).

## Interface graphique (GUI)

Au lancement, un **menu d'accueil** s'affiche :

- champ **Server IP** + **Port** + bouton **Connect** ;
- bouton **Settings** ouvrant un panneau de réglages : transport, client type,
  compression, qualité, sous-échantillonnage, FPS, **résolution** (liste
  déroulante : taille client / 1920x1080 / 1600x900 / 1366x768 / 1280x720 /
  1024x768 / 800x600 / **custom…** avec champ WxH), taille de fenêtre, HW accel,
  plein écran au démarrage, contrôle clavier/souris (KVM), auto-reconnexion,
  logs verbeux ;
- les **réglages sont persistés** dans
  `~/.config/spacedesk-viewer/settings.json` (sauvegardés à chaque modification
  et à la fermeture, rechargés au démarrage) ; les arguments CLI restent
  prioritaires sur les valeurs enregistrées.

En mode vidéo : `Échap` revient au menu, `F11` plein écran.

## Prérequis (Ubuntu 24.04)

```bash
sudo apt install build-essential ffmpeg mesa-va-drivers \
    libxkbcommon-dev libwayland-dev libegl1 libgl1 libx11-6
```

(`mesa-va-drivers` = décodage VA-API Intel/AMD ; pour NVIDIA, utilisez le pilote
propriétaire et éventuellement `--hwaccel none`.)

## Compilation (workspace)

Tout se fait depuis `native/` (workspace Cargo : un seul `target/`, un seul
`Cargo.lock`) :

```bash
cd native
cargo build --release          # spacedesk-viewer + spacedesk-cli -> native/target/release/
cargo test                     # tests protocole/transport/session (hors ligne OK)
cargo clippy --workspace --all-targets   # 0 warning attendu
cargo fmt --check              # formatage vérifié
```

> `core` et `cli` ne dépendent que de `jpeg-decoder` (crates.io) en plus de la
> std ; `gui` utilise `winit 0.29` + `glutin 0.31`/`glutin-winit 0.4` +
> `glow 0.14` + `ab_glyph` + `serde`/`serde_json` (crates.io).

## Utilisation

```bash
# GUI (menu d'accueil ; l'IP peut aussi être passée en argument)
native/target/release/spacedesk-viewer
native/target/release/spacedesk-viewer 192.168.1.10 --fps 60 --fullscreen

# Headless : statistiques + dump des frames décodées (pour valider sans écran)
native/target/release/spacedesk-cli 192.168.1.10 --compression h264 \
    --frames 300 --dump /tmp/frames

# Test complet sans serveur Windows (faux serveur spacedesk en Python)
python3 native/examples/mock_server.py --port 28252 --frames 60 --tcp
native/target/release/spacedesk-cli 127.0.0.1 --compression off \
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
- `FrameBuffer` (serveur→client, type 2) : width@8, height@12 = **taille écran
  complet**, rect@24..36 = **rectangle de la tuile**, compression@40,
  fragmentInfo@64 ; payload = H264/JPEG/raw (une tuile JPEG complète par paquet
  en mode MJPEG).
- `FlowControlAck` (type 7, header 128 octets) envoyé après chaque trame complète
  (ou tous les 4 fragments, comme le viewer HTML5).
- KVM : `Mouse` (10), `Keyboard` (11), `Touch` (12), `Disconnect` (8, 28 octets).

### Validation externe

La lecture du protocole a été croisée avec des implémentations indépendantes :
- [jeromeollivon-star/spacedesk-linux](https://github.com/jeromeollivon-star/spacedesk-linux)
  (client Python sans dépendance) documente les mêmes offsets FrameBuffer
  (8/12 = écran, 24..36 = tuile, 64 = fragmentInfo, tuiles = JPEG complet) ;
- [zkaeshjm/spacedesk-linux](https://github.com/zkaeshjm/spacedesk-linux) (serveur
  Linux compatible) confirme que le port 28252 accepte du **TCP brut binaire**
  ou du **WebSocket** selon les premiers octets.

## Limites connues / pistes d'amélioration

- **Audio** : spacedesk sait forwarder le son du PC hôte vers le viewer (à activer
  dans les réglages du serveur Windows) ; le client ne le décode pas encore —
  piste : nouveau type de paquet + lecture via pipe `ffplay`/ALSA.
- **Zero-copy VA-API → EGL/DMABUF** : au lieu de copier le BGRA de ffmpeg vers
  une texture, exporter les surfaces VA-API en DMABUF et les importer en
  `EGLImage` (interop VA-API/EGL) pour viser le 4K60 sans toucher au CPU.
- **Qualité adaptative** : ajuster dynamiquement `--quality` selon la latence
  mesurée (pire cas : baisser la qualité plutôt que d'accumuler du retard).
- **Entrées tactiles** : le paquet `Touch` (12) est parsé mais les événements
  winit tactiles ne sont pas encore traduits.
- **Manette** : pas de forwarding gamepad (hors scope du protocole KVM souris/clavier).
- Le décodage H264 passe par un pipe vers `ffmpeg` : très rapide jusqu'en
  1440p60 ; testez contre votre serveur Windows réel
  (`spacedesk-cli ... --frames 50` d'abord).
