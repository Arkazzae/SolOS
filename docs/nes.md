# NES emulator (`cores/nes`)

A from-scratch NES (NTSC) core in Rust with no dependencies and no I/O. The caller supplies the cartridge image
and pad state and gets back video, audio and save state. The core is developed on a PC against public test ROMs
and runs on the console inside the game host.

Status (2026-10-30): passes `nestest` and over 70 Blargg test ROMs; runs Super Mario Bros. 3 at a steady 60 fps
on the Zero 2 W.

## How it works

- **Cycle by cycle.** Every CPU bus access is one cycle, and each cycle runs three PPU dots: two before the
  access and one after, which is the most common clock alignment on the real console. Effects that games rely
  on (sprite 0 hit, mid-frame scroll changes, NMI timing) come out as on hardware.
- **CPU (6502 in the 2A03):** all instructions, including the undocumented ones, with the real chip's dummy
  reads and writes. Interrupts are polled at the end of each cycle, and the decision uses the state from the
  second-to-last cycle of the instruction. NMI can hijack IRQ and BRK.
- **PPU:** drawn dot by dot. Background tiles are fetched into shift registers. Sprites for the next line are
  selected from dot 65 (including the hardware bug that makes the overflow flag unreliable) and fetched on the
  dots where the chip fetches them, so a mapper sees the same PPU bus traffic as on the console. The 2C02
  palette includes colour emphasis. Greyscale and emphasis from `$2001` act on the chip's output two dots after
  the pixel is produced, so a change also affects the last two pixels.
- **APU:** two pulse channels, triangle, noise and DMC (samples fetched by DMA from CPU memory), the frame
  counter with IRQ, the console's non-linear mixer, high-pass filters and 48 kHz output.
- **Mappers:** NROM, MMC1, UxROM, CNROM, MMC3 (scanline counter driven by PPU A12) and AxROM, which covers most
  of the NES library. Battery RAM (game saves) can be saved and loaded by the host.
- **Save state:** the whole machine is 15–23 KB. A state carries the ROM hash, so a state from another game or
  a corrupted one is rejected without touching the machine. After loading, the game continues bit for bit
  (video and audio).

## Testing

The test ROMs are not in the repo. `cores/nes/tests/roms/fetch.sh` downloads the `nes-test-roms` collection; then
run `cargo test --release`.

Passing, in over 70 ROMs:

- `nestest`: 8991 instructions match the real console's log, cycle and PPU position included.
- Blargg: `instr_test-v5` (plus `all_instrs` on MMC1), `instr_timing`, `instr_misc`, `cpu_interrupts_v2`,
  `cpu_dummy_writes`, `cpu_reset`, `cpu_exec_space`, `ppu_vbl_nmi`, `vbl_nmi_timing`, `sprite_hit_tests`,
  `sprite_overflow_tests`, `oam_read`, `ppu_open_bus`, the 2005 PPU tests, `apu_test`, `blargg_apu_2005`.
- `mmc3_test_2`, for MMC3 revision B (as in most cartridges).
- Picture test: the `nmi_sync` demo draws a line with per-pixel `$2001` writes, as on the console
  (`tests/picture.rs`).
- Save and load of state on boards with every mapper.

Failing, from the rest of the collection:

- `apu_reset` (APU state after reset).
- `sprdma_and_dmc_dma` (sprite DMA timing when a DMC sample fetch lands inside it).
- `6-MMC3_alt` and `6-MMC6` (other chip revisions).
- ROMs for mappers the core lacks (5, 11, 22, 28, 34).

## Tools

- `cargo run --release --example run -- ROM FRAMES image.ppm [EVERY] [--wav sound.wav]` runs a ROM headless and
  writes frames and audio.
- `--example trace -- ROM FRAMES [EVERY]` prints CPU and PPU state.
- `--example peek -- ROM FRAMES ADDRESS LENGTH` dumps memory, for when a ROM gets stuck.

## Performance

On a PC (i7-12700K) the core runs about 1000 frames per second. For the console, the target is the Zero 2 W's
Cortex-A53: 1 GHz, in-order, roughly one instruction per cycle. The tables below are ARM instructions per frame,
counted under `qemu-arm` with a plugin while the cores were written; that counter was removed on 2026-10-31.
Speed is now measured on the Zero 2 W itself, with a static ARM build of the `run` example, which prints its
frames per second.

| Version | ARM instructions per frame |
|---|---|
| First version with APU | 21.8 M |
| After optimisation (PPU by dot ranges, prepared background and sprite pixels, bank maps, APU computed on change) | 16.4 M |
| PPU catching up in tiles, APU counting down to events | 8.4 M (SMB3: 8.8 M) |

On the Zero 2 W (static `run` binary from the same ARM build), `spritecans` ran at 57 fps and Super Mario
Bros. 3 at 56 fps, about 0.93 instructions per cycle, as the `qemu-arm` count predicted. After the last
optimisation (2026-10-29) they run at 113 and 107 fps:

- **The PPU catches up with the CPU only when the CPU can see or change something.** That means PPU registers,
  mapper banks, OAM, and the dots where the PPU changes interrupt lines (start and end of VBlank, end of frame,
  and for MMC3 every fetch that raises A12, computed exactly from the line's sprites). Catching up still runs dot
  by dot, but visible-line tiles and sprite slots are processed eight dots at a time (each fetch on its own dot,
  for the mapper), and VBlank lines at once.
- **The APU counts down to the cycles that change its output** (end of a playing channel's timer, a frame
  counter step, end of a sample) and catches up arithmetically in between.

The picture is identical frame by frame on 295 test ROMs and on 3000 frames of SMB3; audio is identical bit
for bit.

## On the console

The game host (not published yet) runs NES games.

- **Picture:** 240×224, with no overscan. CRT televisions hid a few pixels at each edge, and games keep things
  there that were never meant to be seen (tiles about to be scrolled in; in SMB3 a column of wrong colours at
  the right edge).
- **Pixel aspect 8:7**, as on an NTSC television. At 1080p the picture is 1097×896: exactly four times the
  frame height, and a horizontal scale of 4.57 with no uneven pixels (a screen pixel that straddles a boundary
  between two blends their colours).
- **Speed:** Super Mario Bros. 3 in level 1-1 holds 60 fps on the Zero 2 W, with the core taking 9.9 ms per
  frame (2026-10-30).
- **Sprite limit off.** The host draws every sprite on a line (`Nes::set_sprite_limit(false)`), so games that
  show more than eight by alternating them do not flicker. A line where the first eight include an empty sprite
  keeps the limit, because games fill the slots on purpose to hide sprites behind them. What the CPU sees
  (sprite 0, the overflow flag, the PPU bus for the mapper) is as on the console, so games behave the same and
  save states work both ways. The core has the limit on by default, as the console does.

## Next

- More mappers (MMC2/MMC4, MMC5, VRC), in the order of the games we play.
