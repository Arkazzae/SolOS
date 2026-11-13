//! The sound, by what comes out: the pitch of the Game Boy channels' notes
//! in a Tonc demo, and a FIFO fed by a few hand-made instructions.

use gba::Gba;

const ROMS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/roms/");

/// The left channel of some frames.
fn record(gba: &mut Gba, frames: u32) -> Vec<i16> {
    (0..frames)
        .flat_map(|_| {
            gba.run_frame(0);
            gba.audio().iter().step_by(2).copied().collect::<Vec<_>>()
        })
        .collect()
}

/// The frequency of a stretch of sound, from its rising zero crossings.
fn pitch(samples: &[i16]) -> f64 {
    let mean = samples.iter().map(|&s| f64::from(s)).sum::<f64>() / samples.len() as f64;
    let rising = samples.windows(2).filter(|w| f64::from(w[0]) < mean && f64::from(w[1]) >= mean).count();
    rising as f64 * f64::from(gba::SAMPLE_HZ) / samples.len() as f64
}

/// snd1_demo plays a tune on the first pulse channel as it starts: D4,
/// E4, A4, D5, each fading.
#[test]
fn pulse_notes() {
    let Ok(rom) = std::fs::read(format!("{ROMS}tonc/bin/snd1_demo.gba")) else {
        eprintln!("skipped: run tests/roms/fetch.sh");
        return;
    };
    let sound = record(&mut Gba::new(&rom).unwrap(), 50);
    let tenth = gba::SAMPLE_HZ as usize / 10;
    let notes: Vec<f64> = [1, 2, 3, 5].iter().map(|&t| pitch(&sound[t * tenth..(t + 1) * tenth])).collect();
    for (got, want) in notes.iter().zip([293.7, 329.6, 440.0, 587.3]) {
        assert!((got - want).abs() < want * 0.04, "notes {notes:.0?}, wanted about {want}");
    }
}

/// A program that turns the sound on, sends FIFO A to both sides at full
/// volume, runs timer 0 to take a sample from it every 1024 cycles (16
/// kHz) and has DMA 1 fill it from the cartridge, -64 and +64 over and
/// over, then waits: out comes a square wave at 8 kHz.
#[test]
fn fifo() {
    let program: [u32; 21] = [
        0xe3a0_0301, // mov r0, #0x04000000
        0xe3a0_1080, // mov r1, #0x80
        0xe1c0_18b4, // strh r1, [r0, #0x84]     SOUNDCNT_X: on
        0xe3a0_1c03, // mov r1, #0x300
        0xe381_1004, // orr r1, r1, #4
        0xe1c0_18b2, // strh r1, [r0, #0x82]     SOUNDCNT_H: FIFO A, 100%, both sides, timer 0
        0xe280_20bc, // add r2, r0, #0xbc
        0xe59f_1028, // ldr r1, [pc, #0x28]
        0xe582_1000, // str r1, [r2]             DMA1SAD: the samples
        0xe280_10a0, // add r1, r0, #0xa0
        0xe582_1004, // str r1, [r2, #4]         DMA1DAD: FIFO A
        0xe59f_101c, // ldr r1, [pc, #0x1c]
        0xe582_1008, // str r1, [r2, #8]         DMA1CNT: on, for the FIFO, repeating, words
        0xe280_2c01, // add r2, r0, #0x100
        0xe3a0_1cfc, // mov r1, #0xfc00
        0xe1c2_10b0, // strh r1, [r2]            TM0CNT_L: 1024 cycles to overflow
        0xe3a0_1080, // mov r1, #0x80
        0xe1c2_10b2, // strh r1, [r2, #2]        TM0CNT_H: start
        0xeaff_fffe, // b .
        0x0800_0100,
        0xb640_0000,
    ];
    let mut rom = vec![0; 0x1000];
    for (i, op) in program.iter().enumerate() {
        rom[4 * i..4 * i + 4].copy_from_slice(&op.to_le_bytes());
    }
    for (i, sample) in rom[0x100..].iter_mut().enumerate() {
        *sample = if i % 2 == 0 { 0xc0 } else { 0x40 };
    }
    let sound = record(&mut Gba::new(&rom).unwrap(), 10);
    let wave = &sound[sound.len() / 2..];
    let (low, high) = (wave.iter().min().unwrap(), wave.iter().max().unwrap());
    assert!(*low < -8000 && *high > 8000, "levels {low} to {high}");
    let hz = pitch(wave);
    assert!((7600.0..8400.0).contains(&hz), "{hz:.0} Hz");
}
