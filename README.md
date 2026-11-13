# Sol OS

A console operating system for small ARM boards: a minimal Linux, its own frontend and its own
emulators, written in Rust.

This repository is being opened part by part, starting with the emulator cores. Each core is a
library with no I/O: the host gives it the game and the buttons, and gets back the picture, the
sound, the cartridge save and save states.

## Cores

| Core | Crate | State | Docs |
|---|---|---|---|
| NES | `emu/nes` | Cycle-exact CPU, PPU and APU; NROM, MMC1, UxROM, CNROM, MMC3, AxROM; passes `nestest` and over 70 Blargg ROMs | [docs/nes.md](docs/nes.md) |
| Nintendo 64 | `emu/n64` | Interpreter and ARM recompiler for the CPU and RSP, software RDP on its own threads, graphics and audio HLE; Super Mario 64, Mario Kart 64, GoldenEye 007, Paper Mario, Ocarina of Time and Donkey Kong 64 play with sound and keep their saves | [docs/n64.md](docs/n64.md) |

The target is full speed on the Raspberry Pi Zero 2 W. The NES runs there at 107–113 fps, the N64
at 60 fps or more in six of the eight measured scenes.

## Building

Stable Rust.

```sh
cd emu
cargo build --release
cargo test --release
```

The test ROMs are not in the repository. Each core's `tests/roms/fetch.sh` downloads the public
suites its tests use; without them those tests skip. N64 games are read from `N64_ROMS`.

To run a game headless and save its picture:

```sh
cargo run --release -p nes --example run -- game.nes 600 out.ppm
cargo run --release -p n64 --example run-n64 -- game.z64 600 out.ppm
```

## License

MIT, see [LICENSE](LICENSE).
