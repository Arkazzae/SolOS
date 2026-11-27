# Sol in a browser (`web/`)

The console's own emulators on a web page. The Rust cores from `cores/` are compiled to WebAssembly and run in
the browser, behind the console's own interface (Start, the Library, Settings, the game menu) rebuilt for a
browser in Svelte. The player adds their own game files. The library and personal saves live in the browser. Single-player works offline.
Two-player NES rooms exchange controller input and temporary machine states through a Rust relay. The app is self-hostable
([web/README.md](../web/README.md)).

The browser plays NES, Mega Drive, Master System, Game Gear, Game Boy, Game Boy Color, Game Boy Advance and
PICO-8. The Master System and Game Gear checks cover cartridge import, rendering, SRAM and Quick Resume.
Both Sega diagnostics were also imported through the file picker and played after an offline reload
in Chromium. Holding Z turned each picture green, and Quick Resume kept that frame at the console’s
native resolution. Native and WebAssembly comparison results for all eight systems are listed below.

The browser tests pass in headless Chromium. Screens were checked at 1440×900 and 390×844.
The built page, served by `vite preview`, was driven by the keyboard through a first run, a dropped
game, the game menu (save, load, start over, leave), the Library, a profile and the language; with its server
stopped it then started again from its service worker and played. Not checked yet: a real pad, Safari,
Firefox, a phone's touch screen and the sound by ear. N64 and PlayStation are not in the page and have not
been tried in a browser.

## Running it

```sh
rustup target add wasm32-unknown-unknown
cd web
pnpm install
pnpm core         # web/core to wasm, into the workspace's target/; dev and build take it from there
pnpm dev          # the Vite dev server (no service worker in dev)
pnpm build        # dist/: a static page and its sw.js; SOL_BASE=/sol/ for a sub-path
pnpm types        # svelte-check and the tools' types, warnings as errors
pnpm check        # the wasm build against the native one, frame by frame (below)
pnpm test         # the browser tests in a fresh headless Chromium or Chrome profile
```

The single-player page needs only a static host: it does not use SharedArrayBuffer, so it does not need
COOP or COEP. It needs a secure page (https, or localhost): `crypto.subtle` (a game's SHA-1), the AudioWorklet
and the service worker exist only there, and the page says so instead of starting elsewhere. The font, icons
and everything else come with the page. The release WebAssembly module with all six cores is about 345 KB after gzip.

## Layout

| Path | What it is |
|---|---|
| `web/core/` | `solweb`, a member of the workspace: the cores behind one interface as plain WebAssembly exports, one file per console in `src/cores/` |
| `web/core/examples/check.rs` | runs a game through the same exports natively and prints hashes |
| `web/index.html`, `src/main.ts` | the page's head (manifest, icons, font) and the element the app fills |
| `src/engine/core.ts` | the module's wrapper, shared by the worker and `tools/check.ts` |
| `src/engine/worker.ts`, `remote.ts`, `protocol.ts` | the machine's worker, the page's side of it, and the messages between them |
| `src/engine/render.ts`, `shaders.ts` | WebGL2 on an OffscreenCanvas: the picture fitted with the console's pixel shape, and the picture looks |
| `src/engine/sound.ts`, `sound-worklet.ts` | the sound: a ring on the audio thread with drift control |
| `src/engine/pace.ts` | how many frames each of the display's refreshes runs |
| `src/engine/session.ts` | a game being played: frames, fast forward, saves and Quick Resume; no DOM but the canvas |
| `src/engine/store.ts` | IndexedDB: games, profiles and each profile's saves |
| `src/engine/input.ts`, `systems.ts` | the pads and the keyboard; the consoles, their files and their buttons |
| `src/lib/text.svelte.ts`, `icons.ts` | English and Polish words; Phosphor icons |
| `src/ui/` | the Svelte app: `App.svelte`, the screens (`screens/`, the game's `Player.svelte` among them), shared parts (`parts/`), the state (`state/`), moving round (`nav.ts`, `layers.ts`, `pump.ts`), the consoles' look (`consoles.ts`) |
| `src/styles/sol.css` | the console's design, in rem |
| `tools/offline.ts` | the Vite plugin that writes `dist/sw.js` after a build |
| `tools/check.ts` | the wasm-against-native check |
| `tools/browser-test.ts`, `persistence-cases.ts` | the browser tests |
| `tools/icons.py` | the installed app's icons, from the Piksel sun |
| `public/` | the font, the icons, the consoles' pictures (`platforms/`), the manifest |
| `Dockerfile`, `Dockerfile.dockerignore`, `Caddyfile`, `compose.yaml` | the self-hosted image: Rust builds the cores, Node the page, Caddy serves it |

## The module

`web/core` has no bindings generator (no wasm-bindgen). It exports plain functions over the module's memory:
`alloc`/`free` for bytes the page writes in, `open(console, rom)`, `run_frame(pad1..pad4)`, `width`, `height`,
`pixel_aspect`, `frame_hz`, `picture` (RGBA), `audio`/`audio_len`/`audio_stereo`/`sample_hz`,
`save_state`/`load_state`, `sram`/`load_sram` and `set_clock`. Bytes that come back (a state, a save, an error
message) wait at `out`/`out_len` until the next such call. A core's panic goes to the browser's console through
an imported `sol.log`.

Each core is a thin adapter (`src/cores/`) that gives the page the picture a TV or the handheld's screen showed:

| Console | Picture | Pixels | Frames a second | Sound | Pads |
|---|---|---|---|---|---|
| NES | 240×224, the 8 overscan pixels at each edge cropped | 8:7 | 60.10 | mono, 48 kHz | 2 |
| Mega Drive | 256 or 320 × 224 (240 on a European console), as the game switches | 8:7 at 256, 32:35 at 320 | 59.92, 49.70 in Europe | stereo, 48 kHz | 2 |
| Master System | 256×192 or 256×224, up to 256×240 on PAL | 8:7 | 59.92 NTSC, 49.70 PAL | stereo output, 48 kHz | 2 |
| Game Gear | 160×144 LCD window, full TV picture in Master System mode | square, 8:7 in Master System mode | 59.92 | stereo, 48 kHz | 1 |
| Game Boy, Game Boy Color | 160×144: four shades in the original's green, or the Color's RGB555 | square | 59.73 | stereo, 48 kHz | 1 |
| Game Boy Advance | 240×160 | square | 59.73 | stereo, 48 kHz | 1 |
| PICO-8 | 128×128, its 32 colours | square | 60 | mono, 22 050 Hz | 4 |

The Game Boy and the Game Boy Color are one core: the cartridge's header says which console it runs on. A
PICO-8 cartridge runs at 30 or 60 frames a second, which shows only once its code has run, so the module always
runs it at 60 and a 30 Hz cartridge makes a frame every other refresh, its sound split between the two.

The buttons go where they sit on the console's own pad. The pad bits are the browser's standard gamepad, which
has the Xbox layout, and the keyboard is the first player's pad:

| Pad | Key | NES, GB, GBC | GBA | Mega Drive | Master System / Game Gear | PICO-8 |
|---|---|---|---|---|---|---|
| bottom face button | Z | B | B | B | 1 | O |
| right face button | X | A | A | C | 2 | X |
| left face button | A | | | A | | X |
| top face button | S | | | Y | | O |
| LB, RB | Q, W | | L, R | X, Z | | |
| Start | Enter | Start | Start | Start | Pause / Start | pause menu |
| Back | Shift | Select | Select | Mode | | pause menu |

The module never asks for the time, because std's clock panics on wasm32-unknown-unknown. The page passes it in
as UTC and the player's offset, at power-on, after a state goes in and before each save. The GBA's cartridge
clock (Pokémon Ruby, Sapphire and Emerald) is set to the local time at power-on and after a state, and then
runs with the game, fast forward included; the Game Boy's MBC3 clock keeps the UTC time beside its save, so it
moves on by the time between; PICO-8 takes its `stat(80)` time of day and, the first time, the seed of
`rnd()`. No test ROM here has a cartridge clock, so none of this is covered by the check below.

The memory can grow at any call that allocates. That makes old typed-array views stale, so `core.ts` makes its
views again after every call. Only the wasm build exports the functions under their plain names: the native
build (`examples/check.rs`) calls them as Rust, where `open`, `close` and `free` under their own names would
stand in for the C library's.

## Checked against the native build

`tools/check.ts` runs the same games in the wasm build under Node and natively (`examples/check.rs`), with the
same button presses and the same fixed clock. It compares a hash of every frame's picture and sound, and the
size of the save state at the end. The wasm build must match the native one bit for bit, the same rule the ARM
builds follow. With no arguments it runs the fetched test ROMs present on disk, generates diagnostic
cartridges for Mega Drive, Master System and Game Gear, and runs PICO-8's `examples/carts/orbit.p8`.

Measured 2026-11-23, 600 frames each:

| Console | Game (test ROM) | Result | wasm / native, frames a second |
|---|---|---|---|
| NES | ny2011 | same | 1799 / 2082 |
| NES | smwstomp | same | 2712 / 2965 |
| NES | nes15-NTSC | same | 2733 / 2794 |
| GB | dmg-acid2 | same | 6202 / 6534 |
| GBC | cgb-acid2 | same | 5302 / 7394 |
| GB | cpu_instrs | same | 3356 / 4626 |
| GBA | tonc m7_demo | same | 2955 / 4059 |
| GBA | tonc snd1_demo | same | 7567 / 10029 |
| GBA | armwrestler-gba-fixed | same | 3166 / 4584 |
| Mega Drive | make-test-rom | same | 2179 / 3012 |
| PICO-8 | orbit | same | 57763 / 89882 |

On 2026-11-27 both Sega diagnostics matched the native build over 600 frames, including stereo sound
and final state size. Master System produced a 256×192 picture and Game Gear produced 160×144.
FluBBa's SMS VDP Test 1.31 also matched for 600 frames.

Short runs measure V8 before it has optimised the module, so the wasm numbers are low; all of them are dozens of
times what a game needs.

## How a game runs

- **Worker.** The module runs in a Web Worker, so a slow frame does not stall the page. Each game gets a fresh
  instance with fresh memory. The Gamepad API exists only on the page's thread, so the page reads the buttons
  on every animation frame and sends them along with the order to run frames.
- **One loop.** The page has one animation-frame loop (`ui/pump.ts`). It reads the pads and the keyboard once a
  frame and gives them either to the game, while one runs with its menu shut, or to the menus as the
  console's actions. The game menu is one of the menus: a layer like a window or a game's page.
- **Picture.** The worker draws on the page's canvas through an OffscreenCanvas with WebGL2. The picture is
  fitted whole with the console's pixel shape, centred on black. At sizes that are not a whole multiple each
  pixel is drawn as the whole multiple below and only the seam between two pixels is blended, so the pixels
  stay sharp and even. The other looks are a CRT TV, an LCD grid and smoothed diagonals.
- **Pace.** If the display refreshes within 1.2 % of the console's frame rate, each refresh runs one frame, as
  the console's game host does on a TV. That covers a 60 Hz screen with every console here but a European
  Mega Drive game, and keeps scrolling even. The sound then plays that much faster or slower. On other displays
  (120 Hz, 144 Hz) frames run as the elapsed time holds, at the console's own rate. If the worker falls far
  behind, the page drops the time instead of rushing to catch up.
- **Fast forward**, as the console's *Przyspieszenie*: the menu's choice (off, 2×, 4×, no limit) for the rest
  of the session, or no limit while Tab or a pad's RT is held (none of these consoles has RT). A refresh asks
  for that many times the frames, no limit for 16, with a budget of 70 % of a refresh: the worker runs a frame
  after the first only while one as long as the last still ends within it, so a slow machine keeps every
  refresh. Only the last frame's sound goes to the worklet, so the ring keeps its level and the sound its
  pitch, broken up. A badge says the speed, top left.
- **Sound.** The worker sends each run's samples straight to an AudioWorklet over a MessageChannel. The
  worklet resamples to the output rate linearly. To hold its ring at about 50 ms it plays up to 0.5 % faster
  or slower. When the ring runs dry (game paused, tab hidden), it goes silent and waits until the ring is full
  again before it plays on.
- **Game menu** (Esc, the pad's guide button, or ⏸ on the bar): back to the game, save and load the state,
  start over; the picture look and fast forward (◂ ▸), the performance counter and full screen (switches);
  leaving. The focused row's line is under the list, and the console's keys beside it. Buttons held when the
  menu closes are kept from the game until they are let go, so the button that chose "Back to the game" does
  not jump or fire.
- **Counter** (Settings › Picture, the game menu, `` ` `` or `?stats` in the address): pictures a second, the
  game's speed against its console's, the measured refresh and which pace is in use, the worker's time per
  run, and the sound ring's level and dropouts.

## Saves and profiles

IndexedDB database `sol`, version 2. A game's key is its file's SHA-1, so adding the same file twice gives one
game, and its saves stay with it. The games are everyone's and the rest is a profile's: a profile's records are
keyed `<profile>/<game>`.

| Store | Content | When it is written |
|---|---|---|
| `profiles` | id (from 1, the highest plus one) and name (up to 24 characters), at most 8 | the first run, Settings › Profiles |
| `games` | filename-based name and tags, console, size, date added, Sega cartridge CRC and a PICO-8 PNG cartridge's own picture | on add, or the first read of an older Sega record |
| `roms` | the game's file, kept apart so the library loads without it | on add |
| `progress` | a profile's game: when played, last frame as PNG, whether a Quick Resume state or a slot exists, favourite | on play, with each snapshot, a star |
| `saves` | the cartridge's own save (battery RAM, flash, EEPROM, PICO-8's cartdata) | every 5 s if it changed, and when the menu opens |
| `resume` | the whole machine, for Quick Resume | every minute, when the tab is hidden, when leaving the game |
| `slots` | the menu's save state, one per game | from the menu |

For Master System and Game Gear the library looks up the CRC-32 in the selected console's local
catalogue. It checks the full dump first, then the payload for files with a 512-byte copier prefix.
On first read, the library computes and caches checksums for previously imported Sega ROMs one at a time.
Later reads only need the shared game record, which keeps the filename-based title as a fallback.
The game ID and profile keys still use the SHA-1 of the complete file, including any prefix.

The web build includes 595 Master System entries and 490 Game Gear entries. The tables contain release
titles and, where known, years, developers and player counts for the original games. To regenerate
`web/src/data/gamedb/{sms,gg}.json` from the Sol TSV tables, run
`python3 web/tools/import-gamedb.py PATH_TO_TABLES` from the workspace root.
[Catalogue attribution and licence](../web/public/gamedb-LICENSE.txt)

Every write is one transaction, and is taken as done only when it has committed: a request's success comes
before the commit. A game's file and its library record go in together. A snapshot commits its changed
cartridge bytes, the machine's state and the library's flags together, or none of them. The worker treats a
save as kept only after the page says its transaction committed, so a failed write is retried with the same
bytes; all saves share one queue. If the save made on leaving fails, the game stays open and says so, and
leaving can be tried again. `tools/persistence-cases.ts` checks each of these against real IndexedDB.

Removing a game removes every profile's records of it; removing a profile only its own. A game for a console
the page does not play stays in the database but not in the library. The current profile and the settings are
in localStorage (`sol-prefs`). A game's page downloads the cartridge's save as a `.sav` and takes one from
another emulator, which also drops the game's Quick Resume, whose machine state holds the old save.

## The interface

The console's screens, in a browser:

- **First run.** The language (English first, Polish), the profile's name (it may stay empty), the first games
  (a file picker or files dropped anywhere; *Later* goes to Start).
- **Start.** The headline column with the hints; the rail of games, the last played or added first, the focused
  card with its A action (Resume with a Quick Resume, else Play) and its menu (≡: the game's page); an *Add
  games* card; the library's consoles with their pictures and epic names, and every game.
- **Library.** Shelves: every game, favourites (once there is one), each console with games. The focused game's
  card on the left, the grid on the right with the letter bar. A card is the last frame the game's Quick Resume
  kept, a PICO-8 cartridge's own picture, or a cover drawn in the console's hue with its title.
- **A game's page** (Menu, `I`, or ≡ on a card): play, start over (asked first), favourite, the save down or up,
  remove (asked first).
- **Settings.** Only what the page does in a browser: Picture, Pads and keyboard (pads connected, the keys in
  games for every console, the keys in menus), Profiles, Games (add, consoles, space used, ask the browser to
  keep them, remove all), Saves (Quick Resume) and System (language, install, offline, version, licences,
  clear everything).

Moving round is the console's: every screen follows a pad's buttons (A, B, X, Y, LB, RB, LT, RT, Menu and the
d-pad, with held directions repeating), and the keyboard stands in for them in the menus: arrows, Enter or
Space (A), Esc or Backspace (B), Q and E (LB, RB), PgUp and PgDn (LT, RT), F (X), R (Y), I (Menu). LB and RB walk
Start, every shelf of the Library and Settings in one line. The focus is the page's own, on real buttons, so the
mouse, touch and Tab work too; the directions move it to the nearest button that way (`nav.ts`), and a window,
a game's page or the game menu takes the buttons while it is open (`layers.ts`). The hints along the bottom show
the keyboard's keys, or the pad's buttons after a pad was used, and can be clicked; touch hides them.

The words are English and Polish (`text.svelte.ts`). The language is reactive state, so switching it redraws
only the words. The lists of consoles in the words are made from the consoles' table (`systems.ts`), as are
the file picker's extensions, the key tables and the console tiles.

The TV's sizes are kept in rem, one rem being ten of the TV's pixels, and the root size follows the window: from
1000×600 up the page is the TV's screen scaled to it (`min(100vw / 192, 100dvh / 108)`), and a smaller window (a
phone) stacks the same parts in a column that scrolls.

## Offline and installing

The manifest (`public/manifest.webmanifest`) lets a browser install the page, full screen, with the Piksel sun
as its icon (`tools/icons.py`); an installed Chromium app also opens the consoles' game files into the library
(`file_handlers`, the launch queue; not `.bin` or `.png`, which other files share). Settings › System offers the browser's install window where it has one.

After a build, `tools/offline.ts` writes `dist/sw.js`: every file of the build, with a hash of their bytes
naming the cache. It answers from the cache first (the page at any address under its scope is the build's
`index.html`), and matches without the response's `Vary`, since module scripts and the font ask with an
`Origin` header the install's requests did not have and some servers answer `Vary: Origin`. A new build
installs beside the running one and takes over at the next start, never under a game.

## Self-hosting

`web/Dockerfile` builds from the repository's root: the cores with Rust 1.96.0 (`--locked`), then the page with
pnpm on Node 24, and serves `dist/` with Caddy (`web/Caddyfile`: a year's caching for the hashed `assets/`,
`no-cache` for the page, its worker and manifest; `SOL_ADDRESS` for HTTPS by name or a home address).
`web/compose.yaml` runs it on port 8080. The image serves the page with `no-cache`, and the module as
`application/wasm`, compressed and cached for a year; the module it builds is byte for byte the one a build
outside Docker makes.

## NES multiplayer

A room belongs to the installation that created it. The invite link includes the installation's base
path and a six-character code. Both players import their own copy of the game. The room checks the
protocol version and the SHA-256 of each browser's emulator module. The guest finds the ROM by its
library SHA-1, so renaming the file is fine. Both players mark themselves ready after opening the
matching ROM. The host can seed the session from their local progress. During the game all snapshots
stay in memory and neither profile's battery save, slot or Quick Resume is written.

- `shared/netplay` defines the wire messages, the server's queue for incomplete frames and the client's
  frame counter and input queues in Rust. The web page instantiates its small WASM module once per room.
  The native types are also available as a Rust library.
- `netplay/server` builds `sol-rooms`. One process holds active rooms and seats in memory. Clients
  connect to `netplay` under the application's base URL. Compose routes that WebSocket through Caddy.
  A connecting guest or returning player also puts the public code in the socket URL so a service
  can route the upgrade to the correct room. The private resume token stays in the first message.
- `netplay/relay` runs the same Rust synchronization state machine on both servers. For the
  Cloudflare demo, `web/cloud` binds a separate Durable Object to each room code and serves the
  browser assets on the same origin. The object accepts hibernatable sockets and keeps seat
  credentials, disconnect deadlines, readiness and the current epoch in its private SQLite-backed
  storage. Frame inputs and emulator snapshots stay transient. When an active game wakes, the
  host seeds a new epoch. The room releases its record on exit or expiration.
  [Cloudflare WebSockets](https://developers.cloudflare.com/durable-objects/best-practices/websockets/)
- Each player submits their NES buttons for numbered frames. The server broadcasts a pair once both
  slots are filled. Each browser queues a few inputs ahead based on observed delay and runs its worker
  with a separate pair per emulated frame. The controller byte goes from A in bit 0 to Right in bit 7.
  The renderer, display refresh and audio device stay local.
- At startup the host sends one machine snapshot. The guest acknowledges loading it before either side
  advances. Each client hashes a fresh machine snapshot every 300 frames. A mismatch, a return from the
  game menu or reconnect starts a new synchronization round. The server discards late inputs from older
  rounds. Each player can reclaim their seat for about a minute after a socket drop while the tab is open.
  Leaving explicitly, or restarting the room server, closes the room.

Run the browser-to-browser check against a running deployment with `pnpm test:rooms` in `web/`.
It uses two fresh Chromium profiles, two display rates and a generated two-controller NES cartridge.
`SOL_TEST_URL` selects the installation (default `http://localhost:8080/`).

### Planned board clients

The earlier `sol-os` tree has three Buildroot targets. The Zero 2 W uses 32-bit ARMv7 userspace.
The Pi 5 and TrimUI Smart Pro image configurations target AArch64. Each image already includes
`solui` (the native menu), `solhost` (the game process) and `sold` (system services including Wi-Fi).
Build the client into the board image. The room server can run on any reachable self-hosted installation.
For a board-to-board game on the same LAN, “Host on this console” could start the native Rust room
server on demand. The other board would join by local address and room code. Ship the small server
binary for the board's Buildroot target so this mode also works on the 32-bit Zero 2 W. Stop it when
the host closes the room. Keep room creation and invitations independent of the console owner's
cloud-save credentials. The earlier `sol-cloud` Worker stores private saves and backups in R2.

In `solui`, the game's page would open the room and the existing on-screen keyboard would take a join
code. At launch, hand the room and assigned seat to `solhost`. Its loop in `host/src/main.rs` already
maps the built-in controls or external pads and calls `core.run_frame_players`. Give each emulated
frame its own number at that call. The current outer loop counts display refreshes and can run zero or
several emulated frames per refresh.

Encode each player's input as the emulated console's buttons. For example, bit zero means Left in
the board's `solui::input::Action` and the bottom face button in the web ABI. Both sides can map to
the same NES buttons before sending them. Use room seats to configure the emulated controller ports
so a guest's local first pad can drive P2. For NES, both builds should pin the same core revision
and state format. Native compatibility needs a tested cross-build identifier in place of the web-only
module digest. Use an in-memory guest save so suspend and exit cannot overwrite the guest's
Quick Resume on the SD card.

First exercise an invite and a reconnect between two browsers on a local Compose server with the
internet disconnected. Then pair a browser with a Zero 2 W on the same installation. Join by code,
exercise both pads over Wi-Fi with a Bluetooth pad on the Pi, reconnect and compare the serialized
NES states at the same frame. For a board-hosted LAN test, let the Pi 5 host and join from the
TrimUI with the internet disconnected and the Compose server stopped. Suspend the handheld during
play and reclaim the same seat on wake.

## Not yet

- N64 and PlayStation, not tried: their recompilers emit ARM code, so in a browser they would run on their
  interpreters, and their drawing threads would have to share the machine's.
- The NES's Four Score and the four-player Mega Drive adapters: the page gives those consoles two pads.
- On-screen buttons for phones and tablets.
- Zipped game files (the browser's DecompressionStream can unpack them without a library).
- Syncing saves with a console.
- An on-screen keyboard for a pad alone (a profile's name is typed on a keyboard or a phone's).
- Files opened from the system into an installed app (the launch queue): written, never run.
- Esc in full screen: Keyboard Lock keeps a short press for the page in Chrome and Edge (holding it leaves full
  screen); not tried, and other browsers leave full screen on the first press.
