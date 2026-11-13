# Sol OS

A console operating system for small ARM boards: a minimal Linux, its own frontend and its own
emulators, written in Rust.

This repository starts with one part of it, the Nintendo 64 core. The rest of the system will
follow.

## Nintendo 64 core

`emu/n64` is a library with no I/O. The host gives it the cartridge image and the buttons, and gets
back the picture, the sound, the cartridge save and save states. It is built in three layers:

- a reference: interpreter CPU, low-level RSP and software RDP, which define correct output;
- fast paths that must match the reference bit for bit: an ARM recompiler for the CPU and the RSP
  (32-bit and 64-bit), NEON for the RSP's vector unit and the rasteriser, the RDP on its own threads;
- high-level emulation of the common graphics and audio microcode.

Super Mario 64, Mario Kart 64, GoldenEye 007, Paper Mario, Ocarina of Time and Donkey Kong 64 boot,
play with sound and keep their saves. The target is full speed on the Raspberry Pi Zero 2 W.

## Building

Rust 1.88 or newer.

```sh
cd emu
cargo build --release -p n64
cargo test --release -p n64
```

The test ROMs are not in the repository. `emu/n64/tests/roms/fetch.sh` downloads the public suites
the tests use; without them those tests skip. Games are read from `N64_ROMS`.

To run a game headless and save its picture after 600 fields:

```sh
cargo run --release -p n64 --example run-n64 -- game.z64 600 out.ppm
```

How the core works, what is tested and how fast it runs: [docs/n64.md](docs/n64.md).

## License

MIT, see [LICENSE](LICENSE).
