mod support;
use sms::{Error, Region, Sms, System};

#[test]
fn master_system_cartridge_executes_interrupts_drives_video_audio_and_pad() {
    let mut sms = Sms::new(&support::sms_rom(), System::MasterSystem).unwrap();
    assert_eq!(
        (sms.system(), sms.region()),
        (System::MasterSystem, Region::Usa)
    );
    sms.run_frame([0, 0]);
    sms.run_frame([0, 0]);
    assert_eq!(sms.dimensions(), (256, 192));
    assert!(sms.frame().iter().all(|&p| p == support::RED));
    // A frame is 800 or 801 samples at 48 kHz.
    assert!(
        (1600..=1604).contains(&(sms.audio().len() / 2 * 2)),
        "{}",
        sms.audio().len()
    );
    assert!(sms.audio().iter().any(|&s| s > 0) && sms.audio().iter().any(|&s| s < 0));
    assert_eq!(sms.take_console_output(), b"OK\n");
    sms.run_frame([sms::BUTTON_1, 0]);
    // The handler saw the button during the frame interrupt and changed colour RAM for the next frame.
    sms.run_frame([sms::BUTTON_1, 0]);
    assert!(sms.frame().iter().all(|&p| p == support::GREEN));
    sms.run_frame([0, 0]);
    sms.run_frame([0, 0]);
    assert!(sms.frame().iter().all(|&p| p == support::RED));
}

#[test]
fn pause_button_is_one_nmi_per_press() {
    let mut sms = Sms::new(&support::sms_rom(), System::MasterSystem).unwrap();
    sms.run_frame([0, 0]);
    sms.take_console_output();
    sms.run_frame([sms::START, 0]);
    sms.run_frame([sms::START, 0]);
    sms.run_frame([0, 0]);
    sms.run_frame([0, sms::START]);
    assert_eq!(
        sms.take_console_output(),
        b"PP",
        "once per press, either pad"
    );
}

#[test]
fn game_gear_cartridge_shows_the_window_in_twelve_bit_colour_and_start_is_not_a_pause() {
    let mut gg = Sms::new(&support::gg_rom(), System::GameGear).unwrap();
    assert_eq!((gg.system(), gg.region()), (System::GameGear, Region::Usa));
    gg.run_frame([0, 0]);
    gg.run_frame([0, 0]);
    assert_eq!(gg.dimensions(), (160, 144));
    assert_eq!(gg.frame().len(), 160 * 144);
    assert!(gg.frame().iter().all(|&p| p == support::RED));
    gg.take_console_output();
    gg.run_frame([sms::START, 0]);
    assert_eq!(
        gg.take_console_output(),
        b"",
        "Start is a port bit on the Game Gear"
    );
    gg.run_frame([sms::BUTTON_1, 0]);
    gg.run_frame([sms::BUTTON_1, 0]);
    assert!(gg.frame().iter().all(|&p| p == support::GREEN));
    // Stereo: both channels carry the tone until port $06 says otherwise.
    assert!(gg.audio().chunks(2).any(|f| f[0] != 0 && f[0] == f[1]));
}

#[test]
fn detect_reads_the_header_and_the_mapper() {
    assert_eq!(
        sms::detect(&support::sms_rom()),
        (System::MasterSystem, Region::Usa)
    );
    assert_eq!(
        sms::detect(&support::gg_rom()),
        (System::GameGear, Region::Usa)
    );
    assert_eq!(
        sms::detect(&support::rom(false, 3)),
        (System::MasterSystem, Region::Japan)
    );
    assert_eq!(
        sms::detect(&support::rom(true, 5)),
        (System::GameGear, Region::Japan)
    );
    let mut codemasters = support::rom(false, 4);
    codemasters[0x7fe6..0x7fea].copy_from_slice(&[0x34, 0x12, 0xcc, 0xed]);
    assert_eq!(
        sms::detect(&codemasters),
        (System::MasterSystem, Region::Europe)
    );
    // Codemasters made Game Gear games too, and no Game Gear is PAL.
    let mut codemasters_gg = support::gg_rom();
    codemasters_gg[0x7fe6..0x7fea].copy_from_slice(&[0x34, 0x12, 0xcc, 0xed]);
    assert_eq!(
        sms::detect(&codemasters_gg),
        (System::GameGear, Region::Usa)
    );
    let mut bare = support::sms_rom();
    bare[0x7ff0..0x7ff8].fill(0);
    assert_eq!(sms::detect(&bare), (System::MasterSystem, Region::Usa));
    assert_eq!(sms::detect(&[1, 2, 3]), (System::MasterSystem, Region::Usa));
    assert_eq!(
        Sms::new(&[], System::MasterSystem).err(),
        Some(Error::NotRom)
    );
    // A few bytes are a program: the Game Gear's Fade Test is 50.
    assert!(Sms::new(&[1, 2, 3], System::MasterSystem).is_ok());
}

#[test]
fn pal_console_runs_a_longer_frame_at_50_hz() {
    let mut pal =
        Sms::with_region(&support::sms_rom(), System::MasterSystem, Region::Europe).unwrap();
    pal.run_frame([0, 0]);
    pal.run_frame([0, 0]);
    // 313 lines of 228 cycles at 3.547 MHz: 965.8 samples a frame, two numbers each.
    assert!(
        (1930..=1934).contains(&pal.audio().len()),
        "{}",
        pal.audio().len()
    );
    assert!(pal.frame().iter().all(|&p| p == support::RED));
}

#[test]
fn a_game_gear_is_never_pal_whatever_the_region() {
    let mut gg = Sms::with_region(&support::gg_rom(), System::GameGear, Region::Europe).unwrap();
    gg.run_frame([0, 0]);
    gg.run_frame([0, 0]);
    assert_eq!(gg.region(), Region::Europe);
    assert!(
        (1600..=1604).contains(&gg.audio().len()),
        "{}",
        gg.audio().len()
    );
}

#[test]
fn snapshots_replay_video_audio_and_machine_state() {
    for (rom, system) in [
        (support::sms_rom(), System::MasterSystem),
        (support::gg_rom(), System::GameGear),
    ] {
        let mut a = Sms::new(&rom, system).unwrap();
        for _ in 0..3 {
            a.run_frame([sms::BUTTON_1, 0]);
        }
        let state = a.save_state();
        let mut b = Sms::new(&rom, system).unwrap();
        b.load_state(&state).unwrap();
        for pads in [[0, 0], [sms::BUTTON_1, sms::START], [0, 0]] {
            a.run_frame(pads);
            b.run_frame(pads);
            assert_eq!(a.frame(), b.frame());
            assert_eq!(a.audio(), b.audio());
            assert_eq!(a.save_state(), b.save_state());
        }
    }
}

#[test]
fn bad_snapshots_and_foreign_games_leave_the_machine_untouched() {
    let mut a = Sms::new(&support::sms_rom(), System::MasterSystem).unwrap();
    a.run_frame([0, 0]);
    let state = a.save_state();
    for end in [0, 1, 6, 8, 14, 100, state.len() - 1] {
        assert!(a.load_state(&state[..end]).is_err());
        assert_eq!(a.save_state(), state);
    }
    let mut extra = state.clone();
    extra.push(0);
    assert_eq!(a.load_state(&extra), Err(Error::BadState));
    let mut other = support::sms_rom();
    other[0x300] = 1;
    assert_eq!(
        a.load_state(&Sms::new(&other, System::MasterSystem).unwrap().save_state()),
        Err(Error::OtherGame)
    );
    // Another console or region is another machine too.
    let gg = Sms::new(&support::sms_rom(), System::GameGear).unwrap();
    assert_eq!(a.load_state(&gg.save_state()), Err(Error::BadState));
    let pal = Sms::with_region(&support::sms_rom(), System::MasterSystem, Region::Europe).unwrap();
    assert_eq!(a.load_state(&pal.save_state()), Err(Error::BadState));
    assert_eq!(a.save_state(), state);
}

#[test]
fn reset_starts_the_cartridge_again() {
    let mut sms = Sms::new(&support::sms_rom(), System::MasterSystem).unwrap();
    for _ in 0..3 {
        sms.run_frame([sms::BUTTON_1, 0]);
    }
    sms.take_console_output();
    sms.reset();
    sms.run_frame([0, 0]);
    assert_eq!(sms.take_console_output(), b"OK\n");
}

#[test]
fn cartridge_ram_is_saved_only_once_a_game_switches_it_on() {
    let mut sms = Sms::new(&support::sms_rom(), System::MasterSystem).unwrap();
    assert!(sms.save_data().is_none());
    assert_eq!(sms.load_save_data(&[0; 10]), Err(Error::SaveSize));
    sms.load_save_data(&vec![7; 0x8000]).unwrap();
    assert_eq!(sms.save_data().map(|d| d[0]), Some(7));
}

/// FluBBa's VDP test (https://github.com/FluBBaOfWard/SmsVdpTest, test 31) expects the V counter to read $C1 in the
/// frame interrupt handler on hardware: the flag is set as line 193 begins, not as line 192's picture ends.
#[test]
fn the_frame_interrupt_handler_reads_line_193_on_either_standard() {
    for region in [Region::Usa, Region::Europe] {
        let mut sms = Sms::with_region(
            &support::rom_reporting_v(false, 4),
            System::MasterSystem,
            region,
        )
        .unwrap();
        for _ in 0..3 {
            sms.run_frame([0, 0]);
        }
        assert_eq!(sms.take_console_output(), b"OK\n\xc1\xc1\xc1", "{region:?}");
    }
    let mut gg = Sms::new(&support::rom_reporting_v(true, 6), System::GameGear).unwrap();
    gg.run_frame([0, 0]);
    gg.run_frame([0, 0]);
    assert_eq!(gg.take_console_output(), b"OK\n\xc1\xc1");
}

/// A Master System cartridge in a European Game Gear, as the Master Gear adapter runs one (and as the Game Gear
/// cartridges of that mode select it): a Master System's colours and whole picture, Start as Pause, still 60 Hz.
#[test]
fn a_game_gear_in_master_system_mode_shows_a_master_system_picture_at_60_hz() {
    let mut gg = Sms::master_system_mode(&support::sms_rom(), Region::Europe).unwrap();
    assert!(gg.sms_mode() && gg.system() == System::GameGear && !gg.pal());
    gg.run_frame([0, 0]);
    gg.run_frame([0, 0]);
    assert_eq!(gg.dimensions(), (256, 192));
    assert!(
        gg.frame().iter().all(|&p| p == support::RED),
        "the colour RAM takes one byte a colour"
    );
    assert!(
        (1600..=1604).contains(&gg.audio().len()),
        "262 lines: {}",
        gg.audio().len()
    );
    gg.take_console_output();
    gg.run_frame([sms::START, 0]);
    assert_eq!(
        gg.take_console_output(),
        b"P",
        "Start is the Pause button's NMI"
    );
    // A cartridge not on the list keeps the Game Gear's own mode, and its states are another machine's.
    let own = Sms::new(&support::sms_rom(), System::GameGear).unwrap();
    assert!(!own.sms_mode());
    assert_eq!(gg.load_state(&own.save_state()), Err(Error::BadState));
}

/// A BIOS image with a game in it starts in the BIOS slot, and looks for a cartridge the way the Master System II's
/// Sonic does: a routine in RAM switches the cartridge slot on and the BIOS off through port $3E, reads, and
/// switches back. With no cartridge the slot reads $FF; the same image taken for a cartridge reads itself and then
/// switches itself off, which is how the Sonic image hung before there was a BIOS slot.
#[test]
fn a_bios_image_runs_in_the_bios_slot_and_finds_the_cartridge_slot_empty() {
    let mut image = vec![0; 0x8000];
    // DI; IM 1; LD SP,$DFF0; JP $0100
    image[0..9].copy_from_slice(&[0xf3, 0xed, 0x56, 0x31, 0xf0, 0xdf, 0xc3, 0x00, 0x01]);
    image[0x100..0x116].copy_from_slice(&[
        0x21, 0x00, 0x02, 0x11, 0x00, 0xc1, 0x01, 0x10, 0x00, 0xed,
        0xb0, // LD HL,$0200; LD DE,$C100; LD BC,16; LDIR
        0xcd, 0x00, 0xc1, // CALL $C100
        0x3e, b'B', 0xd3, 0xfd, // LD A,'B'; OUT ($FD),A
        0x76, 0x18, 0xfd, 0x00, // HALT; JR -3
    ]);
    image[0x200..0x20e].copy_from_slice(&[
        0x3e, 0xab, 0xd3, 0x3e, // LD A,$AB; OUT ($3E),A: the cartridge slot on, the BIOS off
        0x3a, 0x00, 0x00, 0xd3, 0xfd, // LD A,($0000); OUT ($FD),A
        0x3e, 0xe3, 0xd3, 0x3e, // LD A,$E3; OUT ($3E),A: the BIOS back
        0xc9, // RET
    ]);
    let mut bios = Sms::bios(&image, Region::Europe).unwrap();
    bios.run_frame([0, 0]);
    assert_eq!(bios.take_console_output(), [0xff, b'B']);
    // Taken for a cartridge, it reads its own first byte, then the RET lands where nothing answers any more.
    let mut cartridge = Sms::new(&image, System::MasterSystem).unwrap();
    cartridge.run_frame([0, 0]);
    assert_eq!(cartridge.take_console_output(), [0xf3]);
    assert_eq!(
        cartridge.load_state(&bios.save_state()),
        Err(Error::BadState)
    );
    bios.reset();
    bios.run_frame([0, 0]);
    assert_eq!(
        bios.take_console_output(),
        [0xff, b'B'],
        "a reset starts from the BIOS slot again"
    );
}
