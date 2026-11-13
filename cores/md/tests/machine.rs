mod support;
use md::{Error, Md, Region};

#[test]
fn cartridge_executes_interrupts_drives_video_audio_and_pad() {
    let mut md = Md::new(&support::rom()).unwrap();
    md.run_frame([0, 0]);
    md.run_frame([0, 0]);
    assert_eq!(md.dimensions(), (320, 224));
    assert_eq!(md.region(), Region::Usa);
    assert!(md.frame().iter().all(|&p| p == 0xf800));
    assert!((1598..=1604).contains(&md.audio().len()));
    assert!(md.audio().iter().any(|&s| s != 0));
    md.run_frame([md::B, 0]);
    // The vblank handler sampled B and changed CRAM for the next frame.
    md.run_frame([md::B, 0]);
    assert!(md.frame().iter().all(|&p| p == 0x07e0));
    md.run_frame([0, 0]);
    md.run_frame([0, 0]);
    assert!(md.frame().iter().all(|&p| p == 0xf800));
}

#[test]
fn snapshots_replay_video_audio_and_machine_state() {
    let rom = support::rom();
    let mut a = Md::new(&rom).unwrap();
    for _ in 0..3 {
        a.run_frame([md::B, 0]);
    }
    let state = a.save_state();
    let mut b = Md::new(&rom).unwrap();
    b.load_state(&state).unwrap();
    for pads in [[0, 0], [md::B, md::START], [0, 0]] {
        a.run_frame(pads);
        b.run_frame(pads);
        assert_eq!(a.frame(), b.frame());
        assert_eq!(a.audio(), b.audio());
        assert_eq!(a.save_state(), b.save_state());
    }
}

#[test]
fn bad_snapshots_and_foreign_games_leave_the_machine_untouched() {
    let mut a = Md::new(&support::rom()).unwrap();
    a.run_frame([0, 0]);
    let state = a.save_state();
    for end in [0, 1, 6, 14, 100, state.len() - 1] {
        assert!(a.load_state(&state[..end]).is_err());
        assert_eq!(a.save_state(), state);
    }
    let mut extra = state.clone();
    extra.push(0);
    assert_eq!(a.load_state(&extra), Err(Error::BadState));
    let mut other = support::rom();
    other[0x180] = 1;
    assert_eq!(
        a.load_state(&Md::new(&other).unwrap().save_state()),
        Err(Error::OtherGame)
    );
    assert_eq!(a.save_state(), state);
}

#[test]
fn formats_regions_and_save_ram() {
    let mut rom = support::rom();
    rom[0x1f0..0x200].fill(b' ');
    rom[0x1f0] = b'E';
    rom[0x1b0..0x1b4].copy_from_slice(b"RA\xf8\x20");
    rom[0x1b4..0x1b8].copy_from_slice(&0x200001u32.to_be_bytes());
    rom[0x1b8..0x1bc].copy_from_slice(&0x203fffu32.to_be_bytes());
    let mut md = Md::new(&rom).unwrap();
    assert_eq!(md.region(), Region::Europe);
    assert_eq!(md.save_data().unwrap().len(), 8192);
    assert_eq!(md.load_save_data(&[0; 2]), Err(Error::SaveSize));
    let ram = vec![0x5a; 8192];
    md.load_save_data(&ram).unwrap();
    assert_eq!(md.save_data(), Some(ram.as_slice()));
    md.run_frame([0, 0]);
    assert!((1928..=1934).contains(&md.audio().len()));
    let raw_state = md.save_state();
    let mut swapped = rom.clone();
    for word in swapped.chunks_exact_mut(2) {
        word.swap(0, 1);
    }
    let mut m = Md::new(&swapped).unwrap();
    m.load_state(&raw_state).unwrap();
    let mut smd = vec![0; 512 + rom.len()];
    smd[8] = 0xaa;
    smd[9] = 0xbb;
    for i in 0..0x2000 {
        smd[512 + i] = rom[i * 2 + 1];
        smd[512 + i + 0x2000] = rom[i * 2];
    }
    let mut m = Md::new(&smd).unwrap();
    m.load_state(&raw_state).unwrap();
}

#[test]
fn rejects_invalid_images() {
    assert!(matches!(Md::new(&[]), Err(Error::NotRom)));
    let mut rom = support::rom();
    rom[4..8].copy_from_slice(&0x201u32.to_be_bytes());
    assert!(matches!(Md::new(&rom), Err(Error::NotRom)));
    let mut rom = support::rom();
    rom[0x100..0x110].copy_from_slice(b"SEGA 32X        ");
    assert!(matches!(Md::new(&rom), Err(Error::Unsupported(_))));
}
