//! FluBBa's SMS VDP Test 1.31 (https://www.smspower.org/Homebrew/SMSVDPTest-SMS) run on the core. The ROM has no
//! licence and is not in this repository: point `SOL_SMS_VDPTEST` at `VDPTEST.sms` from its zip to run this, else the
//! test does nothing.
//!
//! The program shows a menu; its first entry runs three pages of tests, one more page for each button press, and
//! keeps each test's result in RAM from $C900 (test n at $C900 + n - 1: 0 passed, $FF failed; test 21 holds 1 for a
//! PAL and 2 for an NTSC picture). Tests 46 ("Offscreen Y, col") and 54 ("No disp, spr ovr") are behaviours of the
//! first Master System's VDP, the 315-5124, which the core does not model; the Master System 2's and the Game
//! Gear's do not have them, so those two are expected to fail.
use sms::{Sms, System};

#[test]
fn flubbas_vdp_test_passes_all_but_the_first_chips_two_quirks() {
    let Some(path) = std::env::var_os("SOL_SMS_VDPTEST") else {
        eprintln!("SOL_SMS_VDPTEST is not set; skipping");
        return;
    };
    let rom = std::fs::read(path).unwrap();
    let mut sms = Sms::new(&rom, System::MasterSystem).unwrap();
    for frame in 1..=1000u32 {
        // The first press starts the menu's first entry, the others go on to the next page.
        let pressed = [260, 330, 420, 520]
            .iter()
            .any(|&at| (at..at + 4).contains(&frame));
        sms.run_frame([if pressed { sms::BUTTON_1 } else { 0 }, 0]);
    }
    let result = |n: usize| sms.ram()[0x0900 + n - 1];
    // Tests 20 and 37 to 40 do not exist, and 21 is not a pass or fail.
    let failed: Vec<usize> = (1..=60)
        .filter(|&n| n != 20 && n != 21 && !(37..=40).contains(&n))
        .filter(|&n| result(n) != 0)
        .collect();
    assert_eq!(result(21), 2, "an NTSC picture's V counter values");
    assert_eq!(failed, [46, 54], "the tests that fail");
}
