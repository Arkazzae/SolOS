# Sol in a browser

The Sol console's own emulators on a web page, with the console's interface: Start, the Library and Settings,
driven by a pad or the keyboard. Players add their own game files; games, saves and profiles stay in their
browser. Two-player NES rooms relay pad input and temporary emulator snapshots. Once loaded, the page works offline and can be installed as an app.

Consoles: NES (`.nes`), Mega Drive (`.md`, `.gen`, `.smd`, `.bin`), Game Boy (`.gb`, `.sgb`), Game Boy Color
(`.gbc`, `.cgb`), Game Boy Advance (`.gba`) and PICO-8 (`.p8`, `.p8.png`). No BIOS is needed.

Single-player runs from any static host. For online play, use Docker Compose or the Cloudflare demo Worker.
Both run the same Rust room logic. Players join without accounts.

## Run it with Docker

From this folder:

```sh
docker compose up -d
```

Then open <http://localhost:8080>. The image builds the emulators (Rust to WebAssembly) and the page (Vite),
and serves them with [Caddy](https://caddyserver.com).

### Play NES together

Open the game's details in the library and choose **Play online**. Copy the invitation link or tell the
other player the room code. They can open the link or choose **Join by code** on Start on the same
installation. Both players need the same NES file and emulator build. Each presses **Ready to play**.
Each browser's first gamepad or keyboard controls its assigned seat, P1 or P2.

The host may start with their local progress. The session uses temporary saves for both players.
Opening the game menu pauses the room. A brief network drop reconnects automatically while the tab
remains open. Choosing **Exit** closes the room for both players.

### HTTPS

Browsers let a page keep games, play sound through an AudioWorklet and work offline only on a secure page:
`https://`, or `http://localhost`. To use Sol from other devices, set `SOL_ADDRESS` in `compose.yaml` and
publish port 443:

| `SOL_ADDRESS` | What it is for |
|---|---|
| `:80` (the default) | `http://localhost`, or behind a proxy that already does HTTPS |
| `sol.example.org` | a public name pointing at the server: Caddy gets and renews its certificate |
| `https://192.168.1.20` | an address at home: Caddy makes its own certificate, which each device must trust once |

### Build arguments

| Argument | Default | Meaning |
|---|---|---|
| `SOL_BASE` | `/` | the path the page is served under, e.g. `/sol/` for `https://example.org/sol/` |
| `SOL_COMMIT` | empty | the commit Settings › System shows beside the version |

```sh
docker build -f web/Dockerfile --build-arg SOL_BASE=/sol/ -t sol-web .   # from the repository's root
```

## Cloudflare demo

The Worker in `web/cloud` serves the built page at the root of its domain. Each room gets its own
Durable Object. The Worker has no account or cloud-save dependency. Build the root-path web assets
and the Rust room module locally. From `web/`:

```sh
rustup target add wasm32-unknown-unknown
cargo install --locked wasm-bindgen-cli --version 0.2.127
pnpm install --frozen-lockfile
pnpm core
pnpm build
cd cloud
pnpm install --frozen-lockfile
pnpm dev
```

Open <http://localhost:8787>. To check the bundle locally, run `pnpm exec wrangler deploy --dry-run`
from `web/cloud/`. To publish this separate demo to your account, use `pnpm exec wrangler deploy`
from the same directory. The supplied configuration provisions the room namespace and serves the assets
together. [Durable Object WebSockets](https://developers.cloudflare.com/durable-objects/best-practices/websockets/)

## Host it anywhere else

Build it once, then copy `dist/` to any static host (nginx, Apache, Netlify, Cloudflare Pages, GitHub Pages,
Vercel, an S3 bucket). You need stable Rust with the wasm32 target, Node 22.18 or newer (the tools are
TypeScript that Node runs as it is) and pnpm:

```sh
rustup target add wasm32-unknown-unknown
pnpm install
pnpm core     # the emulator and netplay WASM modules
pnpm build    # the page, to dist/ (SOL_BASE=/sol/ for a sub-path)
```

What the server should do:

- Serve `.wasm` as `application/wasm` (most servers already do).
- Keep `assets/` for a long time (the names carry a hash) and ask again for `index.html`, `sw.js` and
  `manifest.webmanifest` (`Cache-Control: no-cache`), so a new build reaches the players.
- Serve over HTTPS (see above).

No special headers (COOP, COEP) are needed: the page does not use SharedArrayBuffer.

For multiplayer on a separate static host, route the WebSocket at the app's base path (for example
`/sol/netplay`) to `/netplay` on your room server. Build it from the workspace with
`cargo build --release -p sol-rooms`. Its default listen address is `127.0.0.1:8788`, configurable
with `SOL_ROOMS_LISTEN`. A static-only deployment still supports offline solo play.

## Updating

Build and deploy again. A player's browser fetches the new version in the background and starts it the next
time Sol is opened; a game being played is never reloaded.

## What players need to know

- **Where their data is.** In the browser they use, in that browser's storage for this site: games, each
  profile's saves, Quick Resume states and favourites, and the settings. Another browser or device starts
  empty. Clearing the site's data in the browser removes it all.
- **Keeping it.** Settings › Games › Keep the games asks the browser not to clear the games when it runs short
  of room; installing Sol as an app usually gets this too.
- **Saves to and from other emulators.** A game's page (Menu, or `I`) downloads its cartridge save as a `.sav`
  and takes one back.
- **Profiles.** Each person has their own saves, Quick Resume and favourites; the games are shared.

### Controls

The menus follow a pad (A chooses, B goes back, LB/RB change section) and the keyboard stands in for it. In a
game the keyboard is the first player's pad, its buttons where they sit on the console's own pad (Settings ›
Pads and keyboard lists them for every console):

| Key | In the menus | In a game |
|---|---|---|
| Arrows | move | d-pad |
| Enter | choose (A) | Start |
| Esc | back (B) | game menu |
| Q, E | sections, consoles (LB, RB) | |
| PgUp, PgDn | letter (LT, RT) | |
| F, R, I | favourite (X), start over (Y), details (Menu) | F: full screen |
| X, Z | | the pad's right and bottom buttons (A and B on Nintendo's consoles) |
| A, S | | the pad's left and top buttons |
| Q, W | | the bumpers (the GBA's L and R) |
| Shift | | Select |
| Tab | | fast forward while held |
| `` ` `` | | performance counter |

Pads (the standard gamepad mapping: Xbox, PlayStation, Switch Pro and most others) work in both as soon as a
button is pressed. Pads in the order they were connected are players 1 to 4.

## Development

```sh
cargo run -p sol-rooms  # for online play, in a separate terminal
pnpm dev      # the page with live reload (build the emulators once first: pnpm core)
pnpm types    # svelte-check and the tools' types
pnpm test     # browser tests in a fresh headless profile (SOL_BROWSER names the browser's executable)
pnpm check    # the WebAssembly build against the native one, frame by frame
pnpm test:rooms  # two Chromium profiles against Compose, SOL_TEST_URL selects another deployment
```

The design and the internals are in [docs/web.md](../docs/web.md).

## Licences

The emulators are Sol's own (MIT). The font is Figtree (SIL Open Font License, `public/Figtree-OFL.txt`); the
icons are Phosphor (MIT).
