# Sega Mega Drive / Genesis

Sol's own Rust core in `cores/md`, no libretro, split like the other emulators: the library opens no files or devices.
The host hands it the ROM and two pads and takes back an RGB565 picture, 48 kHz stereo PCM, the SRAM and the state of
the whole machine. Early stage, 2026-10-30; the CPUs were checked against two public suites on 2026-11-09 (see
[Conformance suites](#conformance-suites)).

## What is there

- Motorola 68000: the base instruction set, addressing modes, exceptions, user and supervisor stacks, interrupt levels
  4 and 6. Instruction cost is approximate, with no exact prefetch model.
- Z80: the base set, CB/ED/DD/FD, the banked window onto the 68000's memory, 8 KiB of RAM, BUSREQ/RESET and the VBlank
  interrupt.
- VDP Mode 5: VRAM, CRAM, VSRAM, ports and the command latch, DMA from memory, fill/copy, planes A/B, the window,
  scrolling, sprites, palettes, priorities and collisions. It renders by line: a change takes effect from the next
  line. DMA from memory stalls the 68000 for an approximate transfer time.
- PSG: three square channels and noise. YM2612: six FM channels, four operators, algorithms, envelopes, panning,
  timers and the DAC. FM synthesis is approximate: no LFO, SSG-EG, detune or the analogue behaviour of the original
  chip.
- Cartridges: raw `.md`/`.gen`/`.bin`, SMD with the copier header, byte-swapped images, linear ROM and Sega's 512 KiB
  banks, parallel SRAM (also on a single byte lane), write protection. 32X and Pico images and headers pointing at a
  serial EEPROM are refused.
- Two six-button pads, TH, detection of the extended buttons and the sequence timeout. The region comes from the ROM
  header: USA, Japan or Europe; USA is preferred for multi-region ROMs. PAL has 313 lines, NTSC 262.
- Quick Resume: the `SOLMD` format, version and the hash of the normalised ROM, the state of the processors, memories
  and devices. The ROM, the picture and the PCM samples are not included. A bad state is refused without touching the
  running machine.

## Running it

```sh
cargo test -p md
cargo run --release -p md --example run-md -- game.md 600 /tmp/md.ppm
cargo run --release -p md --example run-md -- game.md 600 /tmp/md.ppm \
  --press start@120:6 --press right@180:120 --wav /tmp/md.wav \
  --save /tmp/md.srm --save-state /tmp/md.state
cargo run --release -p md --example run-md -- game.md 60 /tmp/resumed.ppm \
  --load-state /tmp/md.state
```

`--press 2:b@120:30` presses the second pad's B. Frames are numbered from 1. An optional fourth argument saves the
picture every N frames. `--warmup N` skips the first N frames when timing. The output paths are checked so they cannot
overwrite the ROM or the state being loaded. `play-md` plays a cartridge in an SDL2 window with sound and pads.

The repository holds the source of a ROM of its own for diagnostics, with no one else's games or firmware. The VBlank
interrupt reads the pad: B turns the red background green; the PSG plays a tone.

```sh
cargo run -p md --example make-test-rom -- /tmp/sol-md.md
cargo run --release -p md --example run-md -- /tmp/sol-md.md 180 /tmp/sol-md.ppm \
  --press b@30:60 --wav /tmp/sol-md.wav
```

## Host and library

The game host (`solhost`, not published yet) runs the core with `solhost md /games/megadrive/game.md`; the ids
`megadrive` and `genesis` work too. The UI's scanner recognises those folders and the `.md`, `.gen`, `.smd` and `.bin`
files. The host centres the active picture
(256/320 × 224/240) in a fixed 320 × 240 texture, so a VDP mode change does not rebuild the display. For PAL it
repeats one frame in six refreshes of the 60 Hz screen.

Xbox pad: X → A, A → B, B → C, LB → X, Y → Y, RB → Z, Menu → Start, View → Mode. Directions unchanged. The host uses
its existing SRAM saving and Quick Resume in `/data/users/<id>/saves/md/…` (the current
profile's). Sound playback in `solhost` still waits for an
implementation shared by all cores; PCM works in the library and in the WAV export.

## Conformance suites

Without a game to run, the two processors are checked against the public single-step suites, which give a register
and memory state before one instruction and the state after it (the 68000 set also the cycle count and the bus
transactions):

- [SingleStepTests/680x0](https://github.com/SingleStepTests/680x0), `68000/v1`: 125 opcode families, 8,065 tests
  each, 1,000,060 in all (commit `e0d5ece`).
- [SingleStepTests/z80](https://github.com/SingleStepTests/z80), `v1`: 1,604 files (an opcode with its prefixes),
  1,000 tests each, 1,604,000 in all (commit `ebe1875`).

The Z80 is its own crate, `shared/z80`, which the Master System and Game Gear core (not published yet) uses too; its
harness is `shared/z80/src/sst.rs` and `singlestep.rs`, and the 68000's is test-only code in `cores/md/src/sst.rs` and
`cpu/singlestep.rs`. Both add `serde_json` (and `flate2` for the 68000's `.gz` files) as dev-dependencies and nothing to
the library. The data is about 2 GB and stays out of the repository: clone the two suites and point
`SOL_MD_SINGLESTEP` (for the Z80 crate `SOL_Z80_SINGLESTEP`, which also answers to the old name) at a directory that
holds `680x0/` and `z80/`, or put them in `cores/md/tests/singlestep/` and `shared/z80/tests/singlestep/`.
Without the data both tests print a note and pass, so `cargo test -p md -p z80` works anywhere. With it, a wrong final
state fails the run.

```sh
SOL_MD_SINGLESTEP=/path/to/suites cargo test -p md --release single_step -- --nocapture
SOL_Z80_SINGLESTEP=/path/to/suites cargo test -p z80 --release single_step -- --nocapture
```

It takes about 30 seconds for the 68000 and a few seconds for the Z80. `SOL_MD_ONLY=ADD,MOVE.w` limits a run
to the files with those words in their name, `SOL_MD_EXAMPLES=n` prints n failing tests per file, and
`SOL_MD_STRICT=0` reports without failing (`SOL_Z80_ONLY`, `SOL_Z80_EXAMPLES` and `SOL_Z80_STRICT` for the Z80).

| Suite | Tests | Final state | Cycle count |
|---|---|---|---|
| 68000, address errors apart | 821,967 | all agree, bar 3 left out (below) | 113,831 differ (13.8%) |
| 68000, ending in an address error | 178,090 | 178,087 differ, not modelled | not counted |
| Z80, undocumented flag bits 3 and 5 masked | 1,604,000 | all agree | all agree |

What the numbers mean:

- **Address errors.** A word or long access at an odd address ends in an address error on the chip. The suite has
  178,090 such cases (a test is one when its reference reads vector 3). This core rolls the whole instruction back and
  stacks a status word without the instruction's bits and with the wrong I/N bit, where the chip keeps the registers
  the instruction had already changed (a predecrement or postincrement stays done) and raises the error as soon as a
  jump or return lands on an odd address. A game only sees that frame after it has crashed, so it is tallied apart and
  not modelled.
- **Left out.** Two ASL.b tests (`e502 [ASL.b Q, D2]` 1583 and 1761) expect bits 8-31 of the register to change, which
  no byte shift can; the other 8,063 ASL.b tests agree. The third is the only divide by zero in the suite
  (`80ef [DIVU (d16, A7), D0]` 5745): its frame holds the address of the DIVU itself as the stacked PC, where the
  manual says the next instruction, and one sample is not enough to model it. The harness skips all three by name.
- **Cycles.** The 68000's timing is per instruction with approximate effective-address costs. 34 of the 124 files
  have tests ending in another number (14% of the tests in all): PEA and the data-dependent MULU/MULS/DIVU/DIVS, TAS,
  NBCD, JMP/JSR, CHK, LEA, MOVEM, MOVE from SR, bit operations and Scc, and the long forms of ADD, SUB, ADDA, SUBA, AND,
  OR, EOR, CMP, NEG, NEGX, NOT, CLR, TST and MOVE. The other 90 agree to the cycle, among them the byte and word forms of
  ADD/SUB/AND/OR/EOR/CMP, the shifts and rotates, and the branches. A table of costs per opcode and addressing mode could
  be taken from the suite's `length` field. Nothing in the picture or the sound depends on it until a game does.
- **Z80 flags.** The core has no hidden `wz`, `p` or `q` register, so bits 3 and 5 of F (the undocumented X and Y
  flags) are masked out. Compared too (`SOL_Z80_FLAGS=all`), 14,147 tests differ, all in SCF/CCF, `BIT n,(HL)` and
  the repeating block instructions, where those bits come from the hidden registers. The Mega Drive's Z80 only drives
  the sound.
- **What the suites found.** Of the 821,967 68000 tests 16,757 failed at the start. The fixes: ASR past the operand's
  width leaves C and X clear (at the width they are the sign); `LINK A7` pushes A7 as decremented by the push; CHK
  clears Z, V and C; the divide-by-zero trap clears N, Z, V and C (taken from the suite's one divide by zero; the manual calls them undefined); ABCD, SBCD and NBCD follow the chip's correction
  rule for operands that are not BCD digits and set V where the correction flips the sign bit of the plain result. Of
  the Z80's 1,604,000, 3,487 failed: DAA corrects the digits after a subtraction as well as after an addition;
  INIR, OTIR, INDR and OTDR going round again change H and P/V by the carry, bit 7 of the data and B; the undefined
  `ED 77` and `ED 7F` take 8 T-states, not 9.

## Speed

Goal: 100% of console speed (60 fps NTSC, 50 PAL) on the Zero 2 W with room for the host. Measured with SGDK's three
sample ROMs (900 frames after 60 of warm-up, a static `run-md` built for `armv7-unknown-linux-musleabihf` and run on
the Zero 2 W's Cortex-A53 with no picture or sound output, so the host needs a margin on top; the same method as the
other cores). The ROMs come from the SGDK commit named under Other checks and are not in the repository.

| Scene | Zero 2 W before | Zero 2 W now | PC, one thread, before → now |
|---|---|---|---|
| Idle text screen (`hello-world`) | 44.8 | 90.6 | 767 → 1555 |
| Rotating 3D cube (`cube-3D`) | 61.8 | 112.5 | 1044 → 2009 |
| Sonic scene (`game/sonic`, FM and a Z80 driver) | 38.7 | 64.6 | 561 → 912 |

So the first measurement was under 60 even for an idle screen. Switching one part off at a time on the PC showed the
line renderer was 60-70% of a frame, and three changes, each checked to leave the output unchanged:

- **VDP line renderer** (`vdp.rs`). It worked out every pixel of both planes from scratch (the window test, the
  scroll value read from VRAM, divisions by the cell height) and converted each colour with divisions. A plane's line is
  now cut into runs of at most 8 pixels that share one name table cell, one pattern row and one vertical scroll value
  (16-pixel columns and the window's edge cut them too), and the 64 CRAM colours go through a table of 192 RGB565
  values that is rebuilt only when CRAM changes. The old per-pixel renderer stays as `render_line_reference`, test-only,
  and `render_line_matches_the_per_pixel_reference` compares the two bit for bit on 6,000 random VDPs (random
  registers and scroll modes, the window on either side, 32/64/128-cell planes, interlace, shadow/highlight, PAL
  height, sparse and dense patterns, random sprite tables) and on a second line after a CRAM change. Three deliberate
  breakages of the new code each make it fail.
- **Sound chips run lazily** (`lib.rs`). `advance_devices` called the YM2612 and PSG after every 68000 instruction,
  about 100,000 times a frame, a quarter of an idle frame. It now adds the clocks to `sound_debt`, and the chips are run
  through them when the 68000 or the Z80 reads or writes them and at the end of every line.
- **Silent FM channels** (`sound.rs`) are skipped: a channel none of whose operators is sounding adds nothing.

Each was compared with the build before it on the four test ROMs (silence, a PSG tone, Sonic's FM and Z80 driver):
the pictures of 20 runs and the audio of 12 hash the same. That check was a one-off with `run-md --wav`, not a test in
the repository.

What is left, on the PC with parts switched off (Sonic scene): the line renderer is 38% of a frame, and the sound
chips with the Z80 driver together about half. The YM2612 is synthesised in floating point with an `f64` `sin` for
every sounding operator of every one of the 48,000 samples a second, which is the next thing to take down (a table,
with the output compared against the current one); the sprite part of the line renderer is untouched. The Sonic
scene's 65 fps leaves little room over 60 for the host.

## Real games

Four commercial games were run headless with `run-md` on 2026-11-09: Sonic The
Hedgehog (USA, Europe), Sonic The Hedgehog 2 (World, Rev A), Sonic & Knuckles + Sonic The Hedgehog 3 (Europe, the 4 MB
locked-on ROM) and Streets of Rage 3 (USA, 3 MB). All four start and reach play with the right pictures: Sega logo,
title, Green Hill with rings, score and enemies; Emerald Hill with Tails, parallax and water; Sonic 3's intro, Angel
Island Zone with Knuckles; Streets of Rage 3's title, menu and first stage with fighting. Without any input Sonic 2
runs its demo loop, including the two-player split screen, and returns to the Sega logo. No panic, no hang, no
visible garbage, but the pictures were judged by eye with no reference emulator at hand, so a wrong colour, a
misplaced sprite or a single wrong line would not necessarily have been noticed.

Speed on the Zero 2 W in play (a static `run-md` from the build with the speed work above, no picture or sound
output; Start is pressed only before the level, because in a level it is a pause, then right is held and A tapped every
90 frames; the numbers are the frames after the warm-up):

| Game | Play measured (frames) | Zero 2 W | PC, one thread |
|---|---|---|---|
| Sonic The Hedgehog, Green Hill | 600-2800 | 66.0 | 846 |
| Sonic The Hedgehog 2, Emerald Hill | 1500-3400 | 62.9 | 835 |
| Sonic & Knuckles + Sonic 3, Angel Island | 1300-3400 | 63.1 | 767 |
| Streets of Rage 3, first stage | 2000-4000 | 66.7 | 864 |

All four are above the 60 fps of an NTSC game, by 5 to 11%, which leaves no room for the host's own work (the picture
upload, the sound output, the pad), so none of them is at full speed in the host yet.

The sound of the four runs (`--wav`) has no clipping (peaks between 7,700 and 9,300 of 32,767), the music and effects
are stereo and the silent seconds are the screens that have no music; whether it sounds right nobody has listened to.
All four headers say region `JUE`, so the core takes USA, NTSC at 60 Hz, also for the "Europe" ROMs: the 50 Hz path
(313 lines, one frame in six repeated) has not met a game, and needs a PAL-only ROM or a way to choose the region.

## Other checks and limits

The core's own tests cover 68000 instructions and exceptions, Z80 addressing and interrupts, VDP ports, DMA and
composition, PSG/FM, timers, sound banking, pads, ROM formats, regions and SRAM. A cartridge of our own runs 68000 code,
changes a colour through an interrupt and reacts to B. A resume test compares the picture, the PCM samples and the full
state after the following frames. Damaged and foreign snapshots leave the machine unchanged. 50 core tests and 5
integration tests pass.

An independent trial: three ready ROMs from the official
[SGDK repository, commit `9a78a44`](https://github.com/Stephane-D/SGDK/tree/9a78a44143a867cd8c5053b95cd061a21f0b73c2/sample),
300 frames each on the development machine. `basics/hello-world` prints a line, `bitmap/cube-3D` draws a cube,
`game/sonic` shows a scene with a character, palms and water. They are the SDK's examples, not a confirmation that the
commercial Sonic works. That trial found and fixed the decoding of CMPA/ADDA/SUBA with an address-register operand. The
downloaded files live only in the ignored `target/md-homebrew`, not in the sources. A runner built for
`armv7-unknown-linux-musleabihf`, Cortex-A53, ran `hello-world` for 60 frames under QEMU. The full host with DRM/GBM and a
picture on a TV needs a separate check on Linux / a Raspberry Pi.

Still missing: exact FIFO/DMA timing, interlaced output, Mode 4, Sega CD, 32X, EEPROM and extra cartridge chips. Some
undocumented Z80 flags and the FM synthesis are approximate. No PAL-only game has been tried (see below).

Hardware documentation used for the implementation:

- [Sega Genesis Software Development Manual 2.0](https://manuals.plus/m/3b2fd5297e9f819d4e502fadc642703ad2ba7c2f42dffbccbc8ea6b50bce1c30).
- [Sega Genesis Technical Overview](https://www.aidanlawrence.com/other/Misc/SEGA2/Sega2.pdf).
- [Motorola M68000 Programmer's Reference Manual](https://www.nxp.com/docs/en/reference-manual/M68000PRM.pdf).
- [Zilog Z80 CPU User Manual](https://www.zilog.com/docs/z80/um0080.pdf).
