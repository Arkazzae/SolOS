# Sol OS

A console operating system for small ARM boards: a minimal Linux, its own frontend and its own
emulators, written in Rust.

This repository is being opened part by part, starting with the emulator cores. Each core is a
library with no I/O: the host gives it the game and the buttons, and gets back the picture, the
sound, the cartridge save and save states.

## Cores

| Core | Crate | State | Docs |
|---|---|---|---|
| Game Boy, Game Boy Color | `cores/gb` | One core for both, the model picked from the cartridge header; MBC1, MBC2, MBC3 with its clock, MBC5; passes Blargg, Mooneye DMG acceptance, `dmg-acid2` and `cgb-acid2`; no Nintendo boot ROM needed | [docs/gb.md](docs/gb.md) |
| NES | `cores/nes` | Cycle-exact CPU, PPU and APU; NROM, MMC1, UxROM, CNROM, MMC3, AxROM; passes `nestest` and over 70 Blargg ROMs | [docs/nes.md](docs/nes.md) |
| Nintendo 64 | `cores/n64` | Interpreter and ARM recompiler for the CPU and RSP, software RDP on its own threads, graphics and audio HLE; Super Mario 64, Mario Kart 64, GoldenEye 007, Paper Mario, Ocarina of Time and Donkey Kong 64 play with sound and keep their saves | [docs/n64.md](docs/n64.md) |
| PlayStation | `cores/psx` | R3000A with an ARM recompiler (32-bit and 64-bit), GTE, software GPU on its own thread, SPU with XA and CD-DA, MDEC, pads, DualShock, Multitap and memory cards; its own BIOS, so no Sony BIOS file is needed; discs from CHD through `shared/disc` | [docs/psx.md](docs/psx.md) |
| PICO-8 | `cores/pico8` | A compatible console with its own P8 Lua (compiler, register VM, coroutines, metatables, collector), drawing, the four-channel synthesiser and the pause menu; 200 of 200 test carts run and replay from a save state | [docs/pico8.md](docs/pico8.md) |

`shared/` holds what the cores have in common: save states (`savestate`) and disc images (`disc`,
CHD for the PlayStation).

The target is full speed on the Raspberry Pi Zero 2 W. The Game Boy runs there at 170–300 fps, the
NES at 107–113 fps, the PlayStation at 95–112 fps in Crash Bandicoot 2, the N64 at 60 fps or more
in six of the eight measured scenes, and PICO-8's Celeste Classic at 322 fps.

## Building

Stable Rust.

```sh
cargo build --release
cargo test --release
```

The test ROMs are not in the repository. `tests/roms/fetch.sh` in the Game Boy, NES, N64 and
PlayStation cores downloads the public suites their tests use; without them those tests skip. N64
games are read from `N64_ROMS`. PICO-8's tests need nothing extra.

To run a game headless and save its picture:

```sh
cargo run --release -p gb --example run-gb -- game.gbc 600 out.pnm
cargo run --release -p nes --example run -- game.nes 600 out.ppm
cargo run --release -p n64 --example run-n64 -- game.z64 600 out.ppm
cargo run --release -p psx --example run-psx -- game.chd 600 --picture out.ppm
cargo run --release -p pico8 --example run-pico8 -- game.p8.png 600 out.ppm
```

## License

MIT, see [LICENSE](LICENSE). PICO-8's font (`cores/pico8/src/font.bin`) is Lexaloffle's, released
under CC0. PICO-8 is a trademark of Lexaloffle Games; this project is not affiliated with them.
