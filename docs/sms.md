# Sega Master System and Game Gear

One Rust library runs both consoles. The host supplies a cartridge and buttons and reads the picture,
audio and saves. A cartridge starts directly without a BIOS file. The [headless runner](../cores/sms/examples/run-sms.rs)
and the browser use the same core.

## Run a cartridge

From the workspace root, the following commands make and run the two diagnostic cartridges.
Both start with a red screen and a tone. Hold button 1 to turn the screen green.

```sh
mkdir -p target/sms-demo
cargo run --release -p sms --example make-sms-test-rom -- target/sms-demo/demo.sms target/sms-demo/demo.gg
cargo run --release -p sms --example run-sms -- target/sms-demo/demo.sms 120 target/sms-demo/sms.ppm --press 1@119:2 --wav target/sms-demo/sound.wav
cargo run --release -p sms --example run-sms -- target/sms-demo/demo.gg 120 target/sms-demo/gg.ppm
```

Use a game file in place of the diagnostic to play another cartridge. The runner takes the console from
`.sms` or `.gg`, then falls back to the cartridge header. Use `--system sms|gg` or
`--region japan|usa|europe` to override detection. The optional fourth positional argument writes a
picture every N frames. `--save FILE` loads and updates cartridge RAM. `--save-state FILE` and
`--load-state FILE` keep and resume the whole machine.

In the [browser](../web/README.md), add either file to the library. Z and X are buttons 1 and 2.
Enter presses Pause on Master System and Start on Game Gear. Master System accepts a second local
controller. Game Gear uses one.

## The core

Create a machine with `Sms::new(rom, System::MasterSystem)` or `Sms::new(rom, System::GameGear)`.
Call `run_frame([pad1, pad2])` with the held button bits. Read `dimensions()` each frame, `frame()`
for packed RGB565 pixels, and `audio()` for interleaved 48 kHz stereo samples. `frame_hz()` gives the
refresh rate for the machine's video standard.

| Component | Behavior |
|---|---|
| CPU and timing | The [shared Z80](../shared/z80) runs 228 cycles per scanline. NTSC has 262 lines at 59.92 Hz. A PAL Master System has 313 at 49.70 Hz. Game Gear uses NTSC timing in every region. |
| Cartridge | Sega and Codemasters bank switching, up to 4 MiB of ROM and optional 512-byte copier headers. The Sega mapper exposes two 16 KiB banks of cartridge RAM. |
| Video | Mode 4 backgrounds and sprites, scrolling, priority, collision, overflow and line interrupts. Master System draws 256×192 or 256×224, and can draw 256×240 in PAL. Game Gear shows a 160×144 window with a 12-bit palette. Known Game Gear cartridges that select Master System mode get the full TV picture. |
| Sound | SN76489 PSG with three tones and a noise channel. Game Gear can route each channel to the left and right speakers independently. |
| Saves | `save_data()` exposes 32 KiB once the cartridge enables RAM. `save_state()` also keeps the CPU, video, sound and mapper. Loading a state checks the ROM hash, machine and region before replacing the running state. |

The export cartridge header alone does not distinguish Europe from North America. The core defaults to
US timing for generic export cartridges and PAL for Codemasters Master System cartridges.
`Sms::with_region` lets a native host choose explicitly.

## Verification and limits

`cargo test --release -p sms` exercises the CPU bus, both mappers, VDP, PSG, controller input, both
video standards, save RAM and deterministic state replay. The machine tests assemble their own cartridges.
To run the optional [FluBBa VDP Test 1.31](https://www.smspower.org/Homebrew/SMSVDPTest-SMS),
set `SOL_SMS_VDPTEST` to the unpacked ROM when running the test.

```sh
SOL_SMS_VDPTEST=/path/to/VDPTEST.sms cargo test --release -p sms --test vdp_test_rom
```

The VDP test passes all 19 data checks, all 16 miscellaneous checks and 18 of 20 sprite checks.
Cases 46 and 54 cover quirks of the first Master System video chip. The core targets the later chip.
The web [parity check](web.md#checked-against-the-native-build) also runs both diagnostic cartridges.

Earlier headless measurements on a Raspberry Pi Zero 2 W ranged from 409 fps in Balloon Master to
489 fps in Waternet, with rendering to the display and audio playback excluded. Compatibility limits
include mid-scanline palette changes, legacy TMS9918 display modes, YM2413 FM audio, cartridge RAM
on the Codemasters mapper and the Game Gear link cable.
