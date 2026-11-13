# PlayStation (`emu/psx`)

A PlayStation core (SCPH-1001, NTSC) written in Rust, with no I/O of its own, like the NES, Game Boy and
GBA cores. The host supplies a disc (through the `Disc` interface: raw 2352-byte sectors and tracks) or a
bare PS-X EXE, and pad buttons. It gets back video, audio and save states.

Status (2026-10-30): games boot from disc and are playable without a BIOS file, because the core carries
its own BIOS. On the Zero 2 W with the JIT, Crash Bandicoot 2 gameplay runs at about 95 fps and its intro at
about 112 fps.

## Games tested

The core boots the games below from CHD images. Each ran for 40–80 s
in `run-psx` with a few Start and cross presses, under OpenBIOS. "XA" and "CD-DA" mean disc audio that played
during the run.

| Game | Reached | Audio |
|---|---|---|
| Crash Bandicoot | loading N. Sanity Beach | SPU |
| Crash Bandicoot 2 | Warp Room, Crash controllable | SPU |
| Final Fantasy VII (disc 1) | intro with subtitles over video (after a minute of loading) | SPU |
| Gran Turismo | car-maker information screen | silent on that screen |
| Legacy of Kain: Soul Reaver | gameplay, Raziel in 3D | SPU |
| Metal Gear Solid (disc 1) | title screen | SPU |
| Oddworld: Abe's Oddysee | GT Interactive logo video | SPU |
| Persona | menu | XA |
| Rayman | title screen | XA, CD-DA |
| Resident Evil 2 (disc 1) | main menu | SPU |
| Resident Evil 3 | memory card load ("no data") | SPU |
| Silent Hill | content warning | silent on that screen |
| Spyro the Dragon | memory card slot choice | XA |
| Suikoden | menu | XA |
| Tomb Raider II | intro video | XA |
| Twisted Metal | menu | XA, CD-DA |
| WipEout | track select (MDEC intro video before it) | SPU |
| Wipeout XL | menu | XA, CD-DA |

WipEout's cockpit view shows black wedges for a frame at the edges of the near track (2026-11-02, from a
quick resume played on the console). They are the game's: its track routine projects a section as a 5×5 grid
of RTPS vertices, keeps each vertex's GTE FLAG bits 13, 14, 17, 18 and 22 (screen coordinate, divide or depth
saturated), splits a cell with a flagged corner once more, and leaves out each small cell that still has one.
The track's black underside shows through. The interpreter and the JIT, on the PC and the Pi, give the same
frames, and too large triangles are culled by the same rule as DuckStation's (each half of a quad, 1024×512).
A real console was not compared.

What this survey found: RE2, RE3, MGS, Oddworld and Soul Reaver waited in the pad interrupt handler for /ACK
to return high (it was stuck low; on hardware it is a short pulse), and WipEout read the SIO1 baud rate.
Both are fixed.

The same survey with the built-in BIOS (`run-psx GAME.chd` without `--bios`, presses every 120 frames from
frame 400, 4000 frames):

- In gameplay: Crash Bandicoot 1, 2 and 3, Gran Turismo, Persona, Rayman, Soul Reaver, Spyro, Twisted Metal,
  WipEout and Wipeout XL.
- MGS at the title screen, FF7 in the menu, Silent Hill and Oddworld as under OpenBIOS.
- RE2, RE3 and Suikoden read the (empty) memory card: "no files", "no data".
- Tomb Raider II (the disc has a demo launcher) starts the launcher, runs `TOMB2.EXE` through LoadAndExecute
  and reaches the game menu. Returning from the game to the launcher ("loadexec lamer app", loaded by the
  game itself) hangs in a wait loop.
- Boot is a few hundred frames shorter than under OpenBIOS, so the same presses land elsewhere.
- Under `qemu-arm` with the JIT, Crash 2, Spyro, Soul Reaver and MGS produce the same picture at frame 3000
  as the interpreter on a PC.

## How it works

- **CPU (R3000A).** Load delay (a load in flight also completes on an exception), branch delay slots, COP0
  (SR, CAUSE, EPC), exceptions (with a bus error for instruction fetches outside RAM, BIOS and the DMA and SPU
  ports), cache isolation (SR bit 16), KSEG0/KSEG1. Coprocessors 1 and 3 are absent. An instruction takes one
  cycle (code runs from an instruction cache that is not emulated); a read waits for the bus by region (RAM
  4 cycles, ports 2, BIOS and expansion byte by byte), as `cpu/access-time` measures.
- **Bus.** 2 MB RAM, scratchpad, I/O ports, PS-X EXE loading. Interrupts (I_STAT, I_MASK) and DMA channels 0
  (MDEC in), 1 (MDEC out), 2 (GPU), 3 (CD-ROM), 4 (SPU) and 6 (OTC; a looped list never terminates). Timers
  count the system clock, dot clock, lines and 1/8 clock; hblank and vblank sync modes are not implemented.
- **GPU.** GP0/GP1, 1 MB VRAM, software rasteriser: flat, shaded and textured triangles and quads (CLUT
  4/8/15 bit; hardware-style 12-bit fixed point from the leftmost vertex, top-left fill rule), rectangles with
  texture flip, lines and polylines (32.32 step, always dithered), VRAM transfers, dithering, semi-transparency,
  mask bit, texture window, CLUT cache, GPUSTAT. Drawing runs on its own thread (see Performance).
- **GTE (COP2).** All commands (RTPS/RTPT with the hardware UNR division, NCLIP, OP, MVMVA with the FC vector
  bug, NCS, NCCS, NCDS, CC, CDP and their triple versions, DPCS/DPCT, INTPL, DCPL, SQR, AVSZ3/4, GPF, GPL),
  44-bit accumulators with flags, SXY/SZ/RGB queues, sign-extended register reads. `gte/test-all`: 1150 of 1150
  cases match the console.
- **Pads and memory cards (SIO0).** A DualShock in port 1, and in port 2 while the host plugs a second pad in
  (`set_second_pad`; `src/pad.rs`): digital (41h, poll 01h/42h) until a game sends it into its configuration
  mode (43h, ID F3h) and asks for analog mode (44h), when its polls carry the sticks as well (73h, six bytes);
  45h–4Ch answer as a DualShock's do, and 4Dh maps the motors to the poll's bytes (00h the small one, on or
  off, 01h the large one's strength), which `motors(port)` gives the host. A game made for the digital pad never
  leaves digital mode. The pads' mode and map are saved after the rest of a state, so an older state loads too,
  its pads digital. The reply byte arrives after the
  transfer time (8 bits at JOY_BAUD) and ACK with IRQ7 follows shortly, because the BIOS driver depends on it.
  A Multitap (SCPH-1070, `src/tap.rs`, `set_multitap(port, pads)`) in either port, slots A to D: 01h is slot
  A's, unless the access before asked with its third byte (REQ) for the long one (5A80h, then each slot's
  exchange in eight bytes, the game's bytes there that slot's command and parameters, FFh-padded); asked
  again from a long one it is the four-byte garbage one, then long again; 02h–04h reach slots B to D. The
  players then are its slots (`set_player`, `set_player_sticks`, `player_analog`, `player_motors`), and
  its slots' modes go after the DualShocks' in a state. Crash Team Racing drives it as psx-spx says (REQ in
  every access, all four slots read) and stops configuring port 1 as a lone DualShock; it is not yet seen
  counting the slots as players (its VS is skipped in headless runs even with two plain pads). The HLE
  kernel's pad reader (InitPad, PAD_init) has port 2's pad while one is plugged in.
  The memory card (81h; commands R, W, S, write flag, checksum, end byte) is in slot 1, either freshly
  formatted or supplied by the host; the host asks `memory_card_written()` when to save it. SIO1 is registers
  only (games read the baud rate).
- **CD-ROM.** Indexed ports, parameter and response FIFOs, INT1–INT5 one at a time (the next waits for the
  acknowledge). Commands: Getstat, Setloc, ReadN/ReadS (75 or 150 sectors/s), Pause, Init, Stop, MotorOn,
  SeekL/P, Setmode, Setfilter, Getparam, GetlocL/P, GetTN, GetTD, GetID (region from the licence in sector 4),
  Test, ReadTOC, Mute/Demute. The sector buffer feeds the data FIFO and DMA 3.
- **Disc images.** The `emu/disc` crate reads CHD: tracks from CHT2 metadata, 2448-byte frames aligned to four,
  stored or absent pregaps, audio byte-swapped to little-endian. The `chd` crate decompresses (LZMA, zlib,
  FLAC); the core knows nothing about CHD.
- **SPU.** 24 SPU-ADPCM voices with Gaussian interpolation (the 512-entry table from psx-spx), pitch
  modulation, ADSR and volume sweep per psx-spx, noise, reverb (computed every other sample for both sides at
  once, without the 39-tap FIR filter), IRQ9, FIFO and DMA 4 transfers, capture buffers. 44100 Hz; `audio()`
  after each frame; `run-psx --wav` writes it out.
- **Disc audio.** XA-ADPCM (4 and 8 bit, mono and stereo, 37.8 and 18.9 kHz, file and channel filters, zigzag
  resampling to 44.1 kHz from the psx-spx tables) and CD-DA with Play, position reports and auto-pause; ATV0–3
  volumes. Built from the documentation; no game has been heard playing them yet (Crash 2 and the WipEout menu
  play from the SPU).
- **MDEC (video).** Quantisation and scale tables, RLE decoding with zigzag, IDCT on the scale table, YUV→RGB,
  4/8/15/24-bit output; DMA 0 in, DMA 1 out (which waits for decoded data). The WipEout intro (the Psygnosis
  video) plays correctly. Against console captures (`mdec/frame`, 15 bit through DMA) 76% of pixels match
  exactly and the rest differ by 1 in 5 bits; 8-bit blocks (`mdec/8bit`) differ by at most 1. A manual read
  of MDEC0 returns an already arranged 16×16 macroblock, while hardware returns 8×8 blocks in order (DMA 1
  rearranges them), so `frame-15bit` without DMA comes out scrambled.
- **Save state.** `save_state`/`load_state` (about 3.7 MB: RAM, VRAM, sound RAM and the rest), tagged with a
  hash of the game (the disc's volume descriptor, or the EXE). The BIOS, disc and memory cards stay with the
  host. The protocol state in the middle of a memory card byte transfer is not saved. The card's new-card flag
  (bit 3, until the first write after insertion) is part of the machine state, because the card stays in the
  slot: RE3 clears it with a write and must not see a new card after a load. With the built-in BIOS, Crash 2,
  Spyro, WipEout and RE3 (in the middle of a card access) continue identically from a state for 400 frames.

### Built-in BIOS

No BIOS file is needed. The core starts the disc itself (`Psx::with_disc`, `src/boot.rs`: ISO 9660,
SYSTEM.CNF `BOOT` with TCB, EVENT and STACK, or `PSX.EXE`; the EXE is loaded as the Exec call does) and
answers kernel calls itself (`src/bios.rs`).

- **Kernel structures** live in RAM where the Sony kernel keeps them: function tables A, B and C (200h, 874h,
  674h), interrupt handler queues, PCB, TCB and EvCB through the table of tables at 100h. The kernel reads
  them through that table, because a program may rebuild them (as the BIOS does at start; MGS does it with a
  kernel heap: SysInitMemory, SysMalloc, EnqueueTimerAndVblankIrqs, InitDefInt).
- **Interrupts** walk the queues by priority (verifier, then handler), then go to the program's exit
  (SetCustomExitFromException) or return through the TCB. Kernel handlers cover counters (RCnt events,
  ChangeClearRCnt) and pads (InitPad, StartPad, ChangeClearPad, buffers read at vblank). The default handler
  (priority 3) delivers the F00000xxh event of each interrupt, acknowledging per SetIrqAutoAck. Program
  callbacks run on the CPU: the kernel calls back to a ROM address the core answers, and keeps where it was in
  a stack frame.
- **Services:** events, threads, Exec, LoadAndExecute (loads the EXE from disc at once and runs it via Exec
  with the stack it was given), setjmp/longjmp, libc string and memory functions, printf, heap (InitHeap,
  malloc, free, calloc, realloc), rand. PsyQ patches that compute kernel addresses from a table position hit
  traps placed exactly there (`_patch_pad` enables and disables pad reads at B(5Bh)+884h and +894h). The start
  state matches the BIOS: CD controller interrupts enabled (old libcd does not enable them itself), DPCR and
  DICR set, display on.
- **Memory cards.** The driver (InitCard, StartCard, `_card_info`, `_card_load`, `_card_read`, `_card_write`,
  `_new_card`, `_card_status`, `_card_wait`) finishes an operation at vblank, serving the ports alternately
  like the BIOS, directly on the card data and its new-card flag. It delivers the HwCARD (F0000011h) and
  SwCARD (F4000001h) events: done, error, no card, new card. `_card_load` completes in one port step (RE2
  retries after 28 frames).
- **Card files** (`src/files.rs`, `bu00:` and `bu10:`): open (also create with a block count), read, write
  (whole sectors; FASYNC completes with an event at vblank), lseek, close, firstfile and nextfile with `?` and
  `*`, rename, erase, format, errno, on the file system the BIOS writes (directory frames with checksums, 8 KB
  blocks). The device table (DCB at 150h) has tty, cdrom and bu as Sony's does: Soul Reaver's library looks
  for `bu` there and replaces its firstfile. File calls that wait for a card in the BIOS deliver the HwCARD
  event on return, because games wait for it after a firstfile that found nothing. `run-psx --card` saves the
  card; Spyro creates its save on start.
- **Not done:** program-defined devices (AddDevice) and `cdrom:` files.

OpenBIOS (pcsx-redux/nugget, MIT) is the reference. `emu/psx/bios/build-openbios.sh OUT` builds it from
source without a shell: clang with a mipsel target and `ld.lld`, no MIPS gcc. The script patches a copy of the
sources for LLVM: `rfe` as a word, full-form `sltiu`, `li` with `%lo()` as `addiu` (LLVM drops the relocation
there), address 0xfffe0130 as a negative number, alignment in the linker script, fonts kept;
`-mno-check-zero-division`, because `teq` is MIPS II. The core listens to the BIOS putchar as a TTY. Use it with
`run-psx --bios`.

### JIT

On ARM a JIT translates R3000A blocks to the host's code: 32-bit ARM (`compile.rs`, `arm.rs`) on the Pis'
32-bit builds, 64-bit ARM (`compile_a64.rs`, `a64.rs`, since 2026-11-02) on the TrimUI Smart Pro's game host,
which its GPU driver keeps 64-bit, and on 64-bit builds. Elsewhere the interpreter runs, and it stays the
reference the JIT is checked against. Code is in `emu/psx/src/jit/`.

The 64-bit compiler follows the 32-bit one block for block: the same load delay, remembered scratch registers
(W0–W3), code out of line and chaining. What A64 lacks it does otherwise: CSEL and CSET where A32 executes
conditionally, the scratchpad and the code memory in registers of their own (a block takes them as its fifth
and sixth arguments), the cycle count added in one 64-bit instruction. Code memory is RWX on Linux, with the
caches made coherent by `dc cvau`/`ic ivau` at the line sizes `CTR_EL0` gives; on macOS (to try it on a Mac)
it is `MAP_JIT`, made writable only while a block is copied in.

- **Blocks** run from an entry address to a jump or branch, including its delay slot, or until an instruction
  the compiler leaves to the interpreter. Arithmetic, shifts, multiplies, loads and stores, LWL/LWR/SWL/SWR, GTE,
  MFC0/MTC0/RFE, jumps and exceptions are compiled. The block cache is keyed by physical address (2 MB RAM, BIOS ROM).
  A bitmap of code pages invalidates blocks when the CPU or DMA writes to such a 4 KB page and when the
  I-cache is cleared (the BIOS isolates the cache with SR bit 16). Blocks jump straight to their successors
  when no event or interrupt is due.
- **Registers.** The 32 MIPS registers live in a struct in memory; MIPS `r0` is the constant 0. The ARM
  scratch registers R0–R3 remember which MIPS register they hold after a read or store, so the next instruction
  that reads it skips memory. Holding the most-used MIPS registers in ARM r4–r11 is not done yet.
- **Load delay** is resolved at compile time: if the instruction after a load reads its register, the code
  reads the old value from a copy. A load at the end of a block leaves an "in flight" state in the struct,
  and the next block's entry finishes it (LWL/LWR merge with the in-flight value, as in the interpreter).
- **Memory.** RAM takes an inline fast path (address mask, cache-isolation check). Ports, scratchpad and BIOS
  go through a Rust call; scratchpad access is out of line.
- **Timing.** A block adds the sum of its cycles (one per instruction plus read costs) to the counter, then
  compares with `next_event` and returns to the event loop. Interrupts are checked between blocks.
- **Exceptions and the BIOS.** Syscall, break, overflow and address errors leave the block with PC, EPC and
  CAUSE set as in the interpreter. With HLE, the vectors at 80h, A0h, B0h and C0h end the block with a Rust call.
- **Checking.** The tests pass with the JIT under `qemu-arm`, and games produce the same frames as the
  interpreter. `tests/jit.rs` runs 28 programs of `ps1-tests` (CPU, GTE, DMA, GPU, MDEC) 600 frames with the
  JIT and without and compares what they print and the whole VRAM: the same, with the JIT having compiled
  blocks (on 2026-11-02 the 64-bit one on an Apple M-series Mac, natively). The timers' tests are left out:
  the JIT counts a block's cycles at its end and looks at events between blocks, so their counts land a cycle
  or a few apart (909 ticks for 910). A mode that runs every block in both and compares registers and written
  memory is not implemented. The 64-bit JIT on Linux is type-checked (`aarch64-unknown-linux-gnu`) but has not
  run on a board yet.
- **Speed of the 64-bit one** on the Mac (3000 frames, no GPU thread): `gte/test-all` 2.2 s against the
  interpreter's 10.5 s, `gpu/benchmark` 5.9 s against 19.1 s.

## Testing

The test programs are not in the repository. `emu/psx/tests/roms/fetch.sh` downloads `ps1-tests` (JaCzekanski,
release build-158, checked by hash). Their logs and VRAM images come from a real console, so the tests
(`cargo test --release -p psx`) compare the core with hardware:

- Printed text from `psx.log`: `cpu/cop`, `cpu/code-in-io`, `gpu/gp0-e1`, `gpu/mask-bit`; `gte/test-all`
  (1150 of 1150).
- VRAM from `vram.png` (colour hash): `gpu/clipping`, `quad`, `rectangles`, `texture-flip`,
  `texture-overflow`, `transparency`, `triangle`, `uv-interpolation`, `vram-to-vram-overlap`.
- The VRAM tests pass with and without the GPU thread, and the picture of every frame matches in 18 user
  games (`run-psx --hashes`, `--no-gpu-thread`).

The repository tests take the EXE path with an HLE BIOS. Booting OpenBIOS with a disc (`Psx::new` +
`insert_disc`) and building the BIOS are checked by hand (Crash Bandicoot 2), because the game images are not
in the repo.

Still different from the console:

- `gpu/lines`: 64 pixels. The last segment of a closed, shaded polyline starts with a different colour than its
  vertex.
- `gpu/clut-cache`: one row, the palette wrapped past x = 1023.
- `cpu/access-time` and `timers`: results within a cycle or a few cycles, not exact; timer sync modes are not
  implemented.
- `cpu/io-access-bitwidth`: a narrow write to a 32-bit port writes the whole register on the console, and a
  read of empty bits returns what was left on the bus.
- `dma/chain-looping` (timing only), `dma/dpcr` (SPU DMA), and the CD-ROM, SPU and MDEC tests wait for their
  parts.

## Tools

```sh
cargo run --release -p psx --example run-psx -- EXE FRAMES [--vram VRAM.ppm] [--picture IMAGE.ppm]
```

- `run-psx` prints the program's TTY, unhandled BIOS calls and registers. `GAME.chd` in place of the EXE boots
  the disc with the built-in BIOS; `--bios openbios.bin GAME.chd` boots it with that BIOS.
- Other flags: `--wav`, `--card`, `--press`, `--from`, `--hashes` (per-frame picture hashes), `--no-gpu-thread`,
  `--no-jit`, `--counters`, `--samples OUT`.
- `--state FILE` starts from a save state: the game host's quick resume (`/data/users/<id>/saves/ps1/GAME/qr` on the
  console) is one, so a moment someone played can be run again on the PC or the Pi. `--save FILE` writes the
  state after the last frame (RAM starts at byte 449 of it), and `--pictures DIR` writes every frame's picture
  as `DIR/NNNNN.ppm`.
- `cargo run --release -p disc --example sector -- GAME.chd LBA...` shows sectors of an image.
- When a game stalls, `--example trace-psx` shows where (the hottest addresses, or the last instructions
  before a given pc) and writes RAM out for a disassembler.

## Performance

The target is the Zero 2 W (Cortex-A53, 1 GHz). The R3000A runs at 33.87 MHz and the interpreter costs about
90 ARM instructions per MIPS instruction (`gte/test-all` runs 19 fps). PCSX-ReARMed reaches full speed on this
hardware with a MIPS-to-ARM dynarec and a NEON GPU, so this core has a JIT and an optimised rasteriser. The
goal is full speed (60 fps, 50 PAL) in the game host: about 16.7M cycles per frame at 1 GHz, less what the
host needs.

Current numbers (Zero 2 W, `run-psx`):

| Workload | fps |
|---|---|
| Crash Bandicoot 2 intro (average of 1200 frames from BIOS start, OpenBIOS) | 112 |
| Crash Bandicoot 2 gameplay (Warp Room, picture fetched every frame as the frontend does) | 95 |

On the Pi 5 (64-bit) the PS1 core has no dynarec yet, so it runs in the interpreter.

### Progress

Crash Bandicoot 2 intro, ARM instructions per frame at frames 300–400 (counted under `qemu-arm` before that counter was removed on
2026-10-31) and fps on the Pi:

| Version | Instructions/frame | fps |
|---|---|---|
| First | 50.0M | 20 |
| Interrupt line computed on change, RAM as an array, one event comparison, fast fetch | 37.2M | 24 |
| Plus SPU (intro music), disc audio and MDEC | 40.4M | 25 |
| JIT: MIPS blocks compiled to ARM | 9.3M | 65 |
| JIT with GTE, COP0, LWL/LWR; blocks chained | | 81 |

Crash Bandicoot 2 gameplay (Warp Room, frames 7600–8200) is dominated by the GPU:

| Version | Instructions/frame | fps |
|---|---|---|
| Interpreter | 64M | 19 |
| JIT | 31M (rasteriser 18M) | 33 |
| Pixel loops specialised (per shading kind, 32-bit values, colour clamp table) | 27M | 36 |
| Further tuning (rasteriser about 11M, SPU 2.5M) | 20.5M | 41–42 |
| Single thread, picture fetched every frame | | 39 |
| GPU thread, 32-bit triangles, texture depth as a loop constant | | 43 → 71 |

Cycles per frame of the CPU thread and the GPU thread were then read with the Pi's performance counters:

| Change | fps | CPU / GPU Mcycles |
|---|---|---|
| GPU thread, 32-bit triangles, depth as a constant | 71 | 12.1 / 12.6 |
| Picture requested when the game flips the display (right after vblank) instead of at the next one; a packet carries up to eight VRAM rectangles instead of one around everything | 77 | 12.0 / 12.5 |
| Branch-free pixel (a pixel left is written back, dither from a table of zeros); image conversion from row slices | 80 | 12.0 / 12.0 |
| PsyQ VSync wait loop skipped to the next event | 79 | 10.9 / 12.1 |
| Texture window as a loop constant; 4-bit texel without coordinate masks | 88 | 10.9 / 10.9 |
| Image conversion on its own thread; DMA list nodes as RAM slices | 89 | 10.8 / 10.5 |
| Rarely executed JIT code (Rust calls, exceptions, overwritten code) placed after the block | 91 | 10.1 / 10.4 |
| Scratchpad access out of line (no Rust call); whole GP0 commands taken straight from the packet | 95 | 9.5 / 10.1 |

At 91 fps the CPU thread spent half its time in JIT code, 22% in the SPU and 8% in the GTE. The GPU thread
spent 60% in one loop (4-bit textured Gouraud triangles) and 15% in semi-transparent 8-bit ones.

### Techniques

- **GPU thread** (`emu/psx/src/video.rs`). The CPU side tracks each command only for what the CPU reads back
  (GPUSTAT, the GPUREAD latch, the texture page from polygons). Words go in batches to a copy of the GPU that
  draws them in the same order; the GPU is passed through channels, not shared. The machine reads VRAM only
  after that copy has drawn everything and answered (GPUREAD during a VRAM→CPU transfer, saving and loading
  state, `vram()`).
- **Picture hand-off.** The GPU thread takes the picture once no earlier, still-undrawn batch draws in the
  display area (each batch carries up to eight VRAM rectangles it touches). It is requested when the game flips
  the display and at vblank if something has drawn in that area since. Games draw the next frame into the other
  buffer, so the CPU never waits for drawing, and the picture is exactly the one at vblank. A game at 30 fps
  shows the same buffer for two vblanks and it is not taken twice. The GPU thread only copies the display
  rows; a third thread converts them to RGB888. The GPU thread lags at most two frames. The machine computes
  the same with and without the thread.
- **Rasteriser loops.** Triangles use 32-bit arithmetic (edge functions and gradient counters fit, so division
  is one instruction, not a 64-bit call). Pixel loops are specialised per shading kind, with texture depth,
  texture window and 4-bit texels as loop constants; colours saturate with `usat`; the pixel loop is
  branch-free.
- **VSync wait loop** (`emu/psx/src/jit/wait.rs`). PsyQ counts a timeout on the stack until the counter that
  moves the vblank interrupt reaches its target. Profiling showed its two blocks at 11% of CPU-thread cycles.
  Nothing the loop reads can change before the next event, so the dispatcher runs its iterations at once, as
  the compiled blocks would: the same cycles, the same block end, registers and counter as they would be. An
  iteration that would exhaust the timeout takes the normal path.
- **Profiling.** `run-psx --counters` reads the Pi's performance counters per thread (cycles, instructions,
  L1I, L1D and L2 misses, branch mispredictions). `run-psx --samples OUT` samples every 20,000 cycles to find
  where each thread is (addresses for `llvm-symbolizer`, with a map of JIT blocks to MIPS addresses). The
  threads do not wait for data memory, so code is what counts. Instruction counts (the former `qemu-arm`
  profile) can mislead, since they are not cycles.

### Tried and dropped

- Triangle row bounds by division-free steps: division of small numbers is fast on the A53.
- SPU voices computed one by one in batches of samples: the mix in memory instead of registers was slower.
- Scratchpad in JIT code beside the fast path: more L1I misses ate the gain. Out of line it helped.
- Reading the background pixel and the next pixel's texel ahead: the loop got longer and the GPU thread slower.

## On the console

The game host (`solhost`, not published yet) reads CHD discs by sector, uses the core's own BIOS (so the
UI does not require a BIOS file), stores the memory card as the game's save, maps buttons as on a PS pad
(Xbox A is cross), and shows a 4:3 picture in every video mode.

## Next

- Hear and verify XA-ADPCM and CD-DA in a game.
- Program-defined devices (AddDevice) and `cdrom:` files in the built-in BIOS; return from a game to its
  launcher (Tomb Raider II).
- Timer sync modes (hblank, vblank), and exact timing for `cpu/access-time` and `timers`.
- The remaining `ps1-tests` differences listed under Testing.
- The 64-bit JIT run on the TrimUI Smart Pro and a 64-bit Pi 5 build, and measured there.
- Register allocation of MIPS registers in ARM registers in the JIT, and NEON for the GTE where profiles
  point.
- Faster MDEC video decoding (it runs on the CPU).
