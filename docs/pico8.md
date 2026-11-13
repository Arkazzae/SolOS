# PICO-8 (`emu/pico8`)

A PICO-8 compatible console in Rust: its own P8 Lua (parser, compiler, virtual machine and garbage collector),
the drawing API on PICO-8's memory, and the four-channel synthesiser. No external crates, no native Lua, no
code from PICO-8 or other emulators. The core does no I/O: the host gives it the cartridge's bytes and the pads
each frame, and takes the picture, the sound and the 256 bytes of saved data.

Status (2026-10-31): all 200 test carts start and run 400 frames with button presses, and a save state taken
in the middle replays the frames after it bit for bit. On the Zero 2 W most games run several times faster than
they need (see Speed). The game host (`solhost pico8 GAME`, not published yet) runs cartridges with sound,
cartdata saves and Quick Resume; not yet checked on a TV.

## How it works

### P8 Lua

- **Parser** (`parse.rs`): the source is P8SCII bytes, not UTF-8. Lua 5.2 syntax plus PICO-8's: the one-line
  `if (c) …` and `while (c) …`, `?` for print, compound assignment (`+=` to `>><=`), `!=`, `\` (whole division),
  `^^`, the shifts and rotations, the peek operators `@ % $`, `//` comments, binary and fractional hex
  literals, and glyphs in names. Locals are resolved while parsing: each gets a number, and a local a nested
  function uses is marked captured.
- **Compiler** (`compile.rs`): each function becomes register instructions. Locals live in registers, a
  captured one in a cell from the start, so closures share it with no extra work when its scope ends.
  Conditions compile to compare-and-branch instructions; numeric and generic `for`, `goto` and labels,
  varargs, multiple results and method calls each have their own instructions. Multiple assignment stores
  right to left, as the reference Lua does.
- **Machine** (`vm.rs`): one loop runs the instructions. A call to a Lua function pushes a frame and a return
  pops it, without recursing in Rust, so a coroutine is just another thread of frames, and `flip()` can stop the
  main thread in the middle of `_update` and continue from there in the next frame. The console's functions
  that call back into Lua (`foreach`, metamethods) run a nested loop, which a coroutine cannot yield across, as
  in Lua 5.2.
- **Values and heap** (`heap.rs`): a value is a copyable 8-byte tag and number: 16.16 fixed point (`fix.rs`,
  wrapping like PICO-8, division saturating), or the index of a string, table, closure, cell or coroutine in
  the heap's arenas. Strings are interned, so a field lookup compares numbers. Tables have an array part and a
  hash part that keeps insertion order (the order of `pairs`), and remember where the last `next` was, so a
  loop that removes and adds fields keeps its place. A mark-and-sweep collector frees what cannot be reached,
  cycles included, between frames (or during one when the heap grows past 4 MB).
- **Metatables:** `__index`, `__newindex` (tables or functions, chained), `__call`, arithmetic and bitwise
  operators, `__unm`, `__eq`, `__lt`, `__le`, `__len`, `__concat`, `__tostring`, `__pairs`. `_ENV` works as a
  local or a parameter; with a local `_ENV` that lacks them, PICO-8's API functions are still found.
- **Errors** carry the line and, for a nil value, what it was (`global 'x'`, `field 'y'`). A runtime error stops
  the game and is drawn at the bottom of the picture, as PICO-8 shows it; `stop()` ends the game without one.
  A frame that does about 25 million back jumps and calls without finishing counts as stuck.

### The console

- **Cartridges** (`cart.rs`, `png.rs`): `.p8` text (UTF-8 glyphs mapped back to P8SCII) and `.p8.png` images
  (our own PNG and DEFLATE decoder), with plain, `:c:` and PXA-compressed code. `#include` lines and sections
  newer than the runtime needs are skipped.
- **Memory:** 64 KB as on PICO-8: sprites, map, flags, music, sound effects, cartdata, the draw state from
  `0x5f00` and the screen. All drawing state is in RAM (palettes, clip, pen, cursor, camera, fill pattern,
  the last point of `line`, and where sprites, screen and map are), so cartridges that poke it get what they
  expect, and remapping works.
- **Drawing** (`graphics.rs`): pixels, lines, rectangles (also rounded), circles, ovals, sprites with scaling
  and flips, `map` with layers, `tline` with its masks and offsets, fill patterns (with transparency),
  bitplane masks, the screen palette with the 16 secret colours, and screen modes (stretched and mirrored).
  Text uses PICO-8's own font (CC0, Lexaloffle) and P8SCII control codes: colours, background, wide and tall
  characters, cursor moves, tabs, one-off glyphs, custom fonts at `0x5600`, pokes from text.
- **Sound** (`audio.rs`): four channels at 22 050 Hz, one channel out, as zepto8 measured PICO-8's WAV exports
  ([zepto8](https://github.com/samhocevar/zepto8) `src/synth.cpp` and `src/pico8/sfx.cpp`, WTFPL): the eight
  waveforms with their buzz and noiz variants (noise is a random walk that follows the pitch, the phaser two
  triangles 109 to 110 apart), detune as a second wave, the effects (slide from the note before, vibrato a
  quarter of a semitone at 7.5 Hz, drop in frequency, fades, arpeggios at 4 or 8 steps, halved at speed 8 or
  less), custom instruments (sound effects 0–7 at the note's pitch from C2, restarted when the instrument or
  key changes), a 1/130 s crossfade where a note changes sharply, reverb from delay lines of 366 and 732
  samples, dampen as high-shelf filters (2400 Hz −6 dB, 1000 Hz −12 dB), and the registers `0x5f40`–`0x5f43`
  (half rate, reverb, distort, lowpass). Music patterns loop, stop and fade; 8-bit PCM from `serial(0x808)`
  plays at 5512 Hz. A channel at full volume reaches ±0.99 of 16 bits, as in zepto8, so four loud channels can
  clip.
- **Input:** eight players, `btn` and `btnp` with PICO-8's repeat (15 frames, then every 4, set at
  `0x5f5c`/`0x5f5d`).
- **Pause menu:** the pause button (`PAUSE`) opens PICO-8's menu over the picture: continue, the cartridge's
  own items (`menuitem`, whose callbacks get L 1, R 2, X 32 and may keep the menu open), reset cart (its
  saved data kept). The game stands still and is silent while it is open. A cartridge that pokes `0x5f30` keeps
  it shut once and sees `btn(6)` instead.
- **Other functions:** `rnd` and `srand` with the generator's state in RAM, `stat` (memory, CPU, sound state,
  the time of day from the host's clock), `cartdata`/`dget`/`dset`, `menuitem`, `reload` and `cstore` against
  the cartridge's own ROM, `printh` into a buffer. What the console has no part for (keyboard, mouse, other
  cartridges, files, `extcmd`) does nothing rather than stop the game.

### Save states

`save_state()` writes the whole machine: the heap's arenas with their free slots, the globals, the main thread
(a game stopped at `flip()` resumes there), every coroutine, the collector's counters, RAM, the cartridge ROM
(`cstore` can change it), the sound and the input. The code is not in the state; it is compiled again from
the cartridge in the same way, so functions and constant strings keep their numbers. A state carries a sum of
the code, and one of another cartridge, cut short, or pointing outside the heap is refused, leaving the game as
it was. A frame's work is counted from zero, so `stat(0)` and `stat(1)` also come out the same after a load.
States are 0.1–0.4 MB.

## Host API

```rust
let mut p8 = pico8::Pico8::new(&cart_bytes)?;
p8.set_persistent(&saved_256_bytes); // before the first frame, if there is a save
p8.seed(clock as u32);
p8.run_frame([pico8::RIGHT | pico8::O, 0, 0, 0, 0, 0, 0, 0])?;
let pixels = p8.frame(); // 128×128 indices into pico8::PALETTE (32 colours)
let sound = p8.audio();  // i16, one channel, at pico8::SAMPLE_HZ
let fps = p8.fps();      // 30 or 60, known once the code has run
let state = p8.save_state();
```

`pico8::label(bytes)` gives a cartridge's label, 128x128 palette indices: the picture on a `.p8.png` (each pixel
the palette colour nearest to it) or a `.p8`'s `__label__`, a cover with no download. `cartdata_id()` and
`persistent()` give the save, `menu_items()` and `activate_menu_item()` the pause menu a
cartridge registers, `error()` and `ended()` how the game stopped, `take_debug_output()` the `printh` lines.

## In the game host

The host runs every cartridge at 60 Hz: whether a cartridge runs at 30 or 60 shows
only once its code has run, which is after its save is put in. A 30 Hz game makes a frame every other
refresh, with its sound split between the two. 🅾️ is on the pad's bottom and top buttons, ❎ on the right and
left ones (in Celeste, 🅾️ jumps and ❎ dashes), and Menu and View open the pause menu; each pad is a player
(up to four). The cartdata is the cartridge's save, written once the cartridge has called `cartdata()`.

Checked on the Zero 2 W on 2026-10-31 (`SOLHOST_FRAMES`, HDMI display attached, the frontend held off):
Celeste Classic ran at a full 60 refreshes a second with 0.9 ms of core time a frame, POOM at 59.9 with 6.7 ms;
a second run of Celeste came back from its Quick Resume, POOM wrote its cartdata, and the frames each run left
(`qr.ppm`) are the games' title screens as on the PC. Hydra's title, which draws its 8192 pixels in Lua, ran at
30 with 26 ms a frame even with every other `_draw` left out: half speed there until the interpreter is faster.

## Testing

```sh
cargo test --release --manifest-path emu/Cargo.toml -p pico8
cargo run --release --manifest-path emu/Cargo.toml -p pico8 --example run-pico8 -- \
  GAME.p8.png 400 /tmp/last.ppm --wav /tmp/game.wav --press o@100 --press right@120:60 --check-state 250
cargo run --release --manifest-path emu/Cargo.toml -p pico8 --example run-pico8 -- --code GAME.p8.png
```

- Unit tests cover fixed-point numbers, the lexer and parser, tables and the collector, drawing and the font;
  `tests/lua.rs` runs small cartridges for closures, metatables, coroutines, varargs, iteration, strings,
  PICO-8 syntax, `_ENV`, errors and `flip()`; `tests/host.rs` covers frame rates, saves, input repeat, the
  menu, sound, errors, `stop()` and save states (also damaged ones).
- `run-pico8` runs a cartridge headless and writes the last frame as PPM and optionally the sound as WAV.
  `--press BUTTON@FRAME[:FRAMES]` holds `left`, `right`, `up`, `down`, `o`, `x` or `pause`. `--mash SEED` presses buttons at
  random (the same for a seed), a few at a time. `--check-state N` saves after
  frame N and replays the frames after it from the state, which must match. `--code` prints a cartridge's code, `--label CART OUT.ppm` writes its label.
- The 200 test carts are community games and are not in the repository. `run-pico8 --demo` runs the demo cart
  that is (`examples/carts/orbit.p8`).

Survey of 2026-10-31 (400 frames, presses at 100–350, state saved at 250 and replayed): 200 of 200 run and
replay. With `--mash 7` for 3000 frames, 195 run to the end; the other five stop on errors of the games
themselves under input no player gives (a button held from the first frame, a game over before a boss
appears), which PICO-8 would stop on too. The first core of 2026-10-30, a tree-walking interpreter, ran 89 of 200 for 120 frames without
presses.

## Speed

On the Zero 2 W (2026-10-31, `run-pico8 GAME 600 /dev/null --mash 7`, performance governor, no picture or
sound output; the game host adds its own work on top):

| Game | Refresh | fps |
|---|---|---|
| Celeste Classic | 30 Hz | 322 |
| Tetyis | 60 Hz | 177 (the first core: 16) |
| Dank Tomb | 60 Hz | 111 |
| Moonrace | 30 Hz | 94 |
| Into Ruins | 30 Hz | 88 |
| POOM | 30 Hz | 71 |
| Snow! | 30 Hz | 49 |
| Hydra | 60 Hz | 41 in play, 17 on its title |

Hydra's title draws its 8192 pixels one by one in Lua (about 55 million x86 instructions a frame); PICO-8 itself
would not keep 60 Hz there either. For such games the host runs every other frame without `_draw`
(`run_frame_without_draw`) when the last frame with it took more than 85% of its time, as PICO-8 does with a
game over its CPU: the updates keep their speed, the picture comes half as often.

What made the difference on the board: compiled register instructions instead of walking the syntax tree,
values that are copied rather than counted, interned strings, per-instruction hints for global and field
lookups, natives reading their arguments off the stack, the screen turned to pixels two at a time, and
division without 64-bit integer division, which 32-bit ARM does in software (a double division corrected by a
multiplication, shifts and masks for powers of two). Calls of a global `flr`, `abs`, `sgn`, `min`, `max`,
`mid`, `band`, `bor`, `bxor`, `sin`, `cos`, `sget`, `mget`, `fget`, `pget`, `peek`, `poke` or `pset` compile to
an instruction of their own that does the work in place while the global is still PICO-8's function (a game
that redefines one gets its own), and one-colour spans are filled a byte at a time.

Of the 196 carts measured (x86 instructions a frame, 600 frames of random presses), only Hydra (16.2 million
at 60 Hz) is over what the Zero 2 W has for a frame, and runs with every other `_draw`; Skulldude went from
19.6 to 2.5 million with the span filling. x86 instruction counts from `gprofng collect app -h insts`
are a good guide: the Zero 2 W runs about one instruction a clock at 1 GHz.

## Next

- Run games in `solhost` on the TV and check picture, sound and the pad there.
- The interpreter's loop is still about 50 instructions a Lua operation; a register-window pointer and fused
  instructions for global calls would make heavy games like Hydra faster.
- The sound against PICO-8's own output: zepto8's constants are measurements, and its noise and phaser are
  its authors' best guesses.
- `rnd`'s sequence is PICO-8's generator in RAM, but not checked against PICO-8 bit for bit.
- Loading another cartridge (`load`), which multi-cart games use.

## Sources

The [PICO-8 manual](https://www.lexaloffle.com/dl/docs/pico-8_manual.html) (API, memory map,
P8SCII), the [PICO-8 FAQ](https://www.lexaloffle.com/pico-8.php?page=faq) (font and palette are CC0),
[Lua 5.2 reference](https://www.lua.org/manual/5.2/) (semantics), PNG (W3C) and DEFLATE (RFC 1951).
