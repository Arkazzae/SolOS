# Game Boy and Game Boy Color emulator (`cores/gb`)

A from-scratch Game Boy (DMG, the original grey one) and Game Boy Color core in Rust with no I/O. The caller
supplies the cartridge image and pressed buttons and gets back video (four shades, or RGB555 colours on Color),
audio, game saves and save state. The core starts in the state the boot ROM leaves the console in, so no
Nintendo boot ROM is needed. Like the NES core, it is developed on a PC against public test ROMs.

`Gb::new(rom, model)` takes the console model. `Model::detect(rom)` returns Color for games made for Color
(including those that also run on a plain Game Boy) and DMG for the rest, so DMG games run on a DMG with its
accuracy and grey picture. Colourising DMG games in Color compatibility mode is not emulated.

Status (2026-10-30): passes the Blargg, Mooneye (DMG) and acid2 suites listed below; runs on the console inside
the game host.

## How it works

- **Machine cycle by machine cycle.** Every CPU memory access is one machine cycle (four clock ticks). In that
  time the timer counts four ticks, the PPU steps four dots and the APU one cycle.
- **CPU (Sharp SM83):** all instructions with exact timing. The next opcode is fetched as the previous
  instruction ends; an interrupt raised by then takes its place, and HALT repeats that fetch. The HALT bug, the
  EI delay and cancelling an interrupt by writing IE during its dispatch are modelled. Invalid opcodes hang the
  CPU as on hardware.
- **Timer and serial port:** a 16-bit divider whose high byte is DIV. TIMA counts falling edges of one of its
  bits, with delayed reload and with the edges that writes to DIV and TAC produce. The serial port shifts bits
  on the 8192 Hz divider bit; with no second console it reads all ones.
- **PPU:** a line is drawn in one go at the start of mode 3, from the registers at that moment, so mid-line
  register changes (rare in games, common in demos) are not reproduced. PPU timing is cycle-exact for
  everything the Mooneye tests check. The PPU jumps from event to event within a line, and mode 3 is stretched
  by the fine SCX scroll, the window and each sprite, as on the chip. Also modelled: edge-triggered STAT
  interrupts, the LY=LYC latch, LY reading 0 on line 153, the DMG STAT write bug, separate VRAM and OAM lockout
  windows for reads and writes, the blank first frame after enabling the LCD, and the WY latch for the window.
- **APU:** two pulse channels (the first with sweep), wave and noise, clocked by a frame sequencer driven by a
  timer divider bit, with the DMG quirks (an extra length-counter clock, wave RAM reachable only at the moment
  of a read, wave RAM corruption on trigger). Output is stereo with panning and volume, the output capacitor's
  high-pass filter, 48 kHz.
- **Game Boy Color:**
  - 32 KB of banked work RAM, two VRAM banks, background tile attributes (palette, bank, flips, priority), eight
    background and eight sprite palettes in RGB555.
  - Sprite priority by OAM order (or OPRI), with LCDC bit 0 as the master priority.
  - DMA to VRAM, all at once or one block per HBlank.
  - Double speed, switched with STOP (without the pause the chip inserts), the fast serial clock, and the APU
    differences (wave RAM accessible while playing, lengths cleared on power-off).
- **Cartridges:** no controller, MBC1, MBC2, MBC3 (including 4 MB MBC30) and MBC5, which is nearly the whole
  library. A plain Game Boy rejects Color-only games with a distinct error. An MBC5 with a motor (types 1Ch to
  1Eh: Pokémon Pinball, Perfect Dark) runs it with bit 3 of the RAM bank register, the bits under it paging the
  RAM; `rumbling()` says when, for the host to rumble the pad.
- **MBC3 clock** (Pokémon Gold, Silver, Crystal): runs on emulated time and is advanced at every game access, so
  a second lasts exactly that many ticks. The odd register ranges, halting the clock and clearing the
  sub-second counter when seconds are written are modelled.
- **Game saves:** `save_data(unix_time)` returns what the battery keeps: RAM, plus the clock registers and save
  time when there is a clock. The format is BGB's and VBA-M's, so `.sav` files from other emulators fit.
  `load_save_data(data, unix_time)` advances the clock by the time since the save, and `pass_time(seconds)` by
  the time the game was asleep.
- **Frame:** `run_frame` ends at the start of VBlank, so a picture always comes from one frame. With the LCD off
  it runs one frame's worth of time and the screen is blank.
- **Save state:** about 31 KB on DMG and 96 KB on Color, plus cartridge RAM. A state carries the ROM hash, so a
  state from another game, from the other model, or a corrupted one is rejected without touching the machine.
  After loading, the game continues frame for frame (video and audio). The mechanism (the `savestate` crate in
  `shared/savestate`) is shared with the other cores.

## Testing

The test ROMs are not in the repo. `cores/gb/tests/roms/fetch.sh` downloads the `game-boy-test-roms` collection
(c-sp, v7.0); then run `cargo test --release -p gb`.

Passing, in over 200 ROM runs:

- **Blargg:** `cpu_instrs`, `instr_timing`, `mem_timing`, `mem_timing-2`, `halt_bug` and all of `dmg_sound`.
- **Mooneye, DMG acceptance:** timing of all instructions and interrupts, EI, DI and HALT, timer, OAM DMA,
  post-boot-ROM state (DIV phase, registers, serial clock) and all PPU tests.
- **Mooneye, controllers:** MBC1, MBC2 and MBC5.
- **Game Boy Color:** `cgb-acid2` (pixel-exact), all of Blargg's `cgb_sound`, `interrupt_time` (double speed),
  SameSuite DMA and palette tests, and the Mooneye acceptance tests that pass on every model, run on Color.
- **With a reference image:** `dmg-acid2` (pixel-exact), `mbc3-tester`, `rtc3test` (all three parts: basics,
  ranges, writes mid-second), `firstwhite` and the Scribbltests (`lycscx`, `lycscy`, `palettely`, `scxly`,
  `statcount`).
- **Save state and game saves:** state save and load with every controller on both models (also at double
  speed), rejection of foreign and corrupted states, and the clock in the save file (32- and 64-bit time).

Tests with a reference image compare a hash of the shades (or colours) with a hash of the reference PNG. A
picture that does not match is written to `target/tmp`.

Not run or not passing (deliberately, for now): the Mealybug tests and `cgb-acid-hell` (mid-line PPU register
changes), the OAM corruption bug (`oam_bug`), MBC1 multicarts, and the Mooneye `misc` tests that check Color
running DMG games.

## Tools

`cargo run --release -p gb --example run-gb -- ROM FRAMES image.pnm [EVERY] [--wav sound.wav]
[--press BUTTON@FRAME] [--dmg]` runs a ROM headless. It writes frames (PGM, or PPM on Color) and audio, and
prints the CPU registers and whatever the game sent over the serial port.

- `--press start@600` holds a button for 6 frames, to get past a title screen.
- `--dmg` runs a Color-compatible game on a plain Game Boy.

## Performance

On a PC (i7-12700K) the core runs 4000–5000 frames per second. For the console the measure was ARM instructions
per frame under `qemu-arm` (a counter removed on 2026-10-31; see [nes.md](nes.md)):

| ROM | ARM instructions per frame |
|---|---|
| Libbet and the Magic Floor, demo mode, DMG | 4.0 M |
| Libbet, same on Color (the game has its own palettes) | 4.6 M |
| µCity, title screen with scrolling map (Color) | 6.6 M |
| 144p Test Suite, menu (DMG) | 3.8 M |
| `dmg-acid2` | 3.7 M |
| `cgb-acid2` | 4.1 M |
| `dmg_sound` (audio throughout) | 4.1 M |

The Libbet DMG count is frames 700–1200 with `--press select@600 --dmg`.

Measured on the Zero 2 W: `dmg-acid2` 298 fps (3.4 ms per frame), Libbet 274 fps on DMG and 252 fps on Color,
µCity 171 fps (5.8 ms), against a 16.7 ms budget. Game Boy and Color fit on one core with a wide margin.

The cost is mostly work done every machine cycle (APU, timer, PPU).

## On the console

The game host (not published yet) runs Game Boy and Game Boy Color games; the model is chosen with
`Model::detect`. By default the pad's buttons sit where the console's buttons are (the Xbox pad's bottom button is Nintendo's B);
the Buttons in games setting can map them by letter instead.

## Next

- PPU pixel by pixel (FIFO), if we meet a game that changes registers mid-line.
- Optionally, a Color compatibility mode for DMG games (colourising palettes), as a UI setting.
