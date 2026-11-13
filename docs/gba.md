# Game Boy Advance emulator (`cores/gba`)

A from-scratch Game Boy Advance core in Rust with no I/O. The caller supplies the cartridge image and pressed
buttons and gets back a 240×160 RGB555 picture (the same format as Game Boy Color), 48 kHz audio, the contents
of the game's save memory and save state. The core starts in the state the BIOS leaves the console in and has
its own BIOS, so no Nintendo BIOS is needed. Like the NES and Game Boy cores, it is developed on a PC against
public test ROMs and demos.

`Gba::new(rom)` creates the console and `run_frame(buttons)` computes one frame. Buttons are bits in the order
of the KEYINPUT register: A, B, Select, Start, right, left, up, down, R, L (constants `gba::A` … `gba::L`).

Status (2026-10-30): passes the CPU, memory, save and picture tests below; commercial games run in the core at
96–120 fps on the Zero 2 W and at 60 fps in the game host. Cycle-exact timing tests are not passing yet.

## How it works

- **CPU (ARM7TDMI, 16.78 MHz):** instruction by instruction, ARM and Thumb, with the real pipeline (r15 two
  instructions ahead, the opcode fetched in the instruction's first cycle).
  - Each memory access costs what its region costs: bus width, sequential or not, cartridge wait states from
    WAITCNT; a 128 KB ROM boundary forces a non-sequential access.
  - Instructions add their internal cycles (multiply by multiplier bytes, register shifts, LDR).
  - All modes with banked registers, exceptions and interrupts are present, along with the LDM/STM oddities
    (empty list, base in list), rotated reads from unaligned addresses and the PC + 12 store.
- **Cartridge prefetch buffer** (WAITCNT bit 14): while the CPU works elsewhere, the cartridge keeps reading
  up to eight halfwords, and an opcode from the buffer costs one cycle. It is computed lazily, at each ROM
  fetch, from the time the cartridge bus was idle; a data read from ROM stops it.
- **Events, not cycle-by-cycle stepping.** The rest of the console does not run alongside the CPU cycle by
  cycle. The PPU moves at the start of horizontal blank and at the end of a line. Timers move only when their
  overflow does something (an interrupt, a cascading timer, audio); otherwise the counter is computed on read.
  The audio mixer runs 32 768 times a second. The CPU runs to the next event. This is a decision for the Pi:
  at 280 896 cycles per frame, clocking everything every cycle would not fit the budget.
- **Idle loops:** a loop that jumps back with the same registers, writes nothing and reads nothing that changes
  between events (a timer counter, audio state) waits for the event, and the CPU skips to it. A game waiting on
  VCOUNT or DISPSTAT then costs as much as one stopped by HALT.
- **BIOS:** a 16 KB ROM of our own with the exception vectors and interrupt handling. It has the same opcodes at
  the same addresses as Nintendo's, because games and tests read them through BIOS protection: `0xE129F000`
  after startup, `0xE25EF004` in an interrupt, `0xE55EC002` after it, `0xE3A02004` after SWI.
  - The SWI calls are written in Rust: SoftReset, RegisterRamReset, Halt, Stop, IntrWait, VBlankIntrWait, Div,
    DivArm, Sqrt, ArcTan, ArcTan2 (the BIOS's own polynomials), CpuSet, CpuFastSet, GetBiosChecksum,
    BgAffineSet, ObjAffineSet (sines from a 256-entry 1.14 table, as in the BIOS), BitUnPack, LZ77, Huffman and
    RL decompression (to WRAM and VRAM), the difference filters, SoundBias and MidiKey2Freq.
  - IntrWait waits as in the BIOS: after every interrupt the call restarts until the game's handler marks the
    awaited interrupt at `0300_7FF8`.
  - BIOS copies do not read the BIOS.
  - Missing: the BIOS sound driver (SWI 0x1A–0x24, 0x28–0x2A; games carry their own in ROM) and MultiBoot.
- **Memory:** the full map with mirrors, and open bus (the last fetched opcode; in Thumb, halfwords combined by
  region, as GBATEK describes). Byte writes to palettes and background VRAM are written as halfwords, and are
  ignored for sprite VRAM and OAM. The bitmap-mode VRAM mirror that does not exist is not mirrored.
- **PPU:** a line is drawn in one go at the start of horizontal blank, from the registers at that moment, so
  mid-line register changes are not reproduced (effects from HBlank interrupts and HBlank DMA, line by line,
  are).
  - Modes 0–5: text backgrounds in 16 and 256 colours up to 512×512, affine backgrounds with and without
    wrapping, and 240×160 and 160×128 bitmaps with two pages, also rotated.
  - Regular and affine sprites (double size), 1D and 2D mapping, semi-transparent sprites, the sprite window,
    the per-line cycle budget.
  - Windows 0 and 1, alpha blending, brighten and darken, background and sprite mosaic.
- **DMA:** four channels, started immediately, in VBlank, in HBlank, for the audio FIFO and for video capture
  (channel 3). Repeat, address reload, interrupt, the last-transfer value read from the BIOS, and the transfer
  length that determines the EEPROM size are modelled.
- **Timers:** four, with dividers 1, 64, 256 and 1024, cascading, interrupts and feeding samples to the FIFO.
- **Audio:** the four Game Boy channels (those from `cores/gb`, adapted: two wave RAM banks, 75% volume,
  computed in bulk up to the moment of a read, a write or a mixer sample) and two 8-bit sample channels fed from
  32-byte FIFOs by DMA on timer overflow. The mixer is the console's: 32 768 Hz, the SOUNDCNT_H shares, bias
  and clipping to 10 bits, then interpolation to 48 kHz and a high-pass filter. The higher mixer rates from
  SOUNDBIAS are not reproduced.
- **Save memory:** the type is detected from Nintendo's library string in the ROM (`SRAM_V`, `FLASH_V`,
  `FLASH512_V`, `FLASH1M_V`, `EEPROM_V`), as other emulators do.
  - SRAM 32 KB.
  - Flash 64 KB (Panasonic ID) and 128 KB (Sanyo, with bank switching), with write, erase and ID commands.
  - EEPROM 512 B or 8 KB (size from the first DMA, or from the size of the loaded file).
  - `save_data()` and `load_save_data()` use the raw format of mGBA and VBA-M, so `.sav` files from other
    emulators fit.
- **Cartridge clock** (Seiko S-3511A: Pokémon Ruby, Sapphire, Emerald, Boktai): the GPIO port at `0800_00C4`
  with reset, status (12/24 h mode), date-with-time and time-only commands, in BCD. A cartridge with a clock is
  recognised by the string `SIIRTC_V`. The host sets the time (`set_clock(seconds)`, local, since 1970); after
  that it follows emulated time, and `pass_time(seconds)` adds the time spent asleep.
- **Rumble cartridges** (Drill Dozer, WarioWare: Twisted!, known by their game codes V49 and RZW): the motor
  is pin 3 of the same port, running while the game sets it as an output and high; `rumbling()` says when.
  WarioWare's gyroscope is not emulated.
- **Frame:** `run_frame` ends at the start of VBlank, so a picture always comes from one frame (1/59.73 s).
- **Save state:** about 400 KB plus save memory. A state carries the ROM hash, so a state from another game or
  a corrupted one is rejected without touching the machine. After loading, the game continues frame for frame
  (video and audio). The mechanism (crate `savestate`) is shared with the NES and Game Boy cores.
- **Serial port:** registers remember writes; there is no second console.

## Testing

The test ROMs are not in the repo. `cores/gba/tests/roms/fetch.sh` downloads the jsmolka and alyosha-tas
collections, the NanoBoyAdvance hardware tests, FuzzARM and ARMWrestler (each from a pinned commit), and the
Tonc demos (checked by SHA-256). Then run `cargo test --release -p gba`.

Passing:

- **jsmolka:** `arm`, `thumb`, `memory` (mirrors, byte writes to video memory), `bios` (BIOS open-bus values),
  `nes`, `unsafe` and the saves (`sram`, `flash64`, `flash128`, `none`). A test ends in a loop with the number
  of the first failure in r12.
- **FuzzARM:** five ROMs of 10 000 random ARM and Thumb instructions each (data processing, multiplies,
  transfers) with results from a real console.
- **ARMWrestler:** all six groups (ALU, LDR/STR, LDM/STM in ARM and Thumb), with no "BAD" on any page.
- **Picture tests:** jsmolka's `hello`, `shades` and `stripes`, and 26 Tonc demos: all modes, affine
  backgrounds and mode 7 from HBlank interrupts, a window shaped by HBlank DMA, affine sprites, windows,
  blending, mosaic, BIOS calls (sin, sqrt, atan and div plots; ArcTan2 octants) and timer cascade. There are no
  real-console images for the demos: the hashes come from this core's own images, each inspected and compared
  with what the demo should draw according to its source. The test guards against changes.
- **Save state and save memory:** state save and load with PSG and FIFO audio, per-line interrupts, HBlank DMA,
  timers, IntrWait, SRAM and Flash; rejection of foreign and corrupted states; save memory moved between
  machines.
- **Audio:** the Tonc `snd1_demo` melody comes out at the right pitches (D4, E4, A4, D5, within 4%), and FIFO A
  fed by DMA 1 on timer 0 overflow (a small ARM program inside the test) produces a full-amplitude 8 kHz square
  wave.
- **Unit tests:** the EEPROM protocol (block write and read, size from DMA length) and the clock (date, time
  after emulated time passes, 12-hour mode, leap year).

Not passing (deliberately, for now): the cycle-exact timing tests from NanoBoyAdvance's hw-test and the
alyosha-tas collection (DMA and timer start delay, HALTCNT timing, interrupt latency, exact PPU flag timing,
prefetch boundaries). They are not run in the test suite.

## Tools

`cargo run --release -p gba --example run-gba -- ROM FRAMES image.ppm [EVERY] [--wav sound.wav]
[--press BUTTON@FRAME]` runs a ROM headless. It writes frames (PPM) and audio, and prints the CPU registers.
`--press start@60` holds a button for 6 frames.

## Performance

On a Mac with an Apple M5 the core runs 1000–2700 frames per second on the Tonc demos. For the console the
measure was ARM instructions per frame under `qemu-arm` (a counter removed on 2026-10-31; see
[nes.md](nes.md)), frames 60–360:

| ROM (Tonc) | ARM instructions per frame |
|---|---|
| `swi_vsync`: affine sprite, waiting with VBlankIntrWait | 3.6 M |
| `brin_demo`: scrolling text background, waiting on VCOUNT | 5.6 M |
| `win_demo`: two windows and the sprite window | 7.8 M |
| `obj_aff`: affine sprite, recomputed every frame | 9.2 M |
| `bld_demo`: alpha blending of a sprite and a background with the background below | 11.6 M |
| `m7_ex`: mode 7, per-line interrupt, sprites | 12.7 M |

At one instruction per cycle on a 1 GHz Cortex-A53 that is 4–13 ms per frame against a 16.7 ms budget.
Commercial games cost more than demos (their own audio mixer, game logic), so the margin has to be checked on
the Pi. The cost is mostly CPU interpretation and composing picture lines.

In the table above, instructions per frame is the difference between the counts for 360 and 60 frames,
divided by 300.

Speed on the console is measured on the Zero 2 W itself, with a static ARM build of `run-gba` and a game
copied to its card.

### On the Zero 2 W (2026-10-29)

Commercial games on one 1 GHz core, frames 1800–2400 after getting through the menu
(`--press start@700 --press a@900 ...`), before and after optimisation:

| Game | Before | After |
|---|---|---|
| Super Mario Advance 4 (world map) | 66 fps | 120 fps |
| Pokémon Emerald (intro) | 66 fps | 102 fps |
| Golden Sun (first scene) | 65 fps | 99 fps |
| Advance Wars (name entry) | 33 fps | 96 fps |

What helped most:

- Idle-loop detection that also works when the loop calls functions (stack writes below the stack pointer do
  not count) and keeps waiting through events that change nothing for it.
- A prefetch-buffer model without 64-bit division (a Cortex-A53 in 32-bit mode divides in software).
- Backgrounds painted straight onto the canvas, with the top layer and the one below it, skipping transparent
  tiles.

Each change was compared frame by frame against the old core on eight games.

The games execute 25–50 thousand ARM7 instructions per frame, and the interpreter spends about 150 Cortex-A53
instructions on each. If a game turns out heavier, the next step is cheaper opcode decoding and fetching.

## On the console

The game host (not published yet) runs GBA games. Advance Wars and Golden Sun hold 60 fps there; across those
two and Super Mario Bros. 3 the core takes 6–11 ms per frame. Golden Sun saves its flash memory.

## Next

- If a game drops below full speed (60 fps in the game host): a cheaper interpreter (table-driven ARM and
  Thumb decoding, cheaper opcode fetch).
- Timing accuracy per hw-test and alyosha-tas (DMA, timers, HALTCNT, interrupt latency), if we meet a game that
  needs it.
- Pixel-by-pixel PPU for mid-line register changes, the higher mixer rates, the BIOS sound driver, MultiBoot.
