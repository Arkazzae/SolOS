//! The library of the sound microcodes' routines this knows. A routine is known by its fingerprint
//! (recognise.rs: its code and the code it reaches, without jump targets and branch offsets), so the
//! same routine is the same entry wherever a build puts it and whatever command leads to it; an entry
//! says what the routine does and how its code reads a command's words: the packing, the DMEM base it
//! adds, whether it takes RDRAM addresses through SEGMENT's table, the constants its code has (where it
//! finds the codebook, the resampling filter, ENVMIXER's eighths), what it differs in where builds
//! differ, and the RSP's cycles it takes. A routine whose code differs but whose behaviour does not
//! lists more than one fingerprint. A new build of a microcode needs the fingerprints of its new
//! routines here, and their behaviour where it differs.

use super::adpcm::{Adpcm, AdpcmWords, LoadAdpcm};
use super::buffers::{ClearBuff, DmemMove, DmemMove2, SetBuff, SetLoop, Transfer, TransferWords};
use super::envelope::{EnvMixer, EnvSetup, Ramp, Ramping, RampingWords, SetValue, SetVol, Value};
use super::filters::{Filter, Polef};
use super::mixing::{HiLoGain, Interleave, InterleaveWords, MixCount, MixOrder, Mixer};
use super::resample::{Downsample, Resample, ResampleWords};
use super::{align, State};
use crate::N64;

/// How a routine's code takes an RDRAM address from a command word.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(super) enum Rdram {
    /// Through SEGMENT's table: the segment in the top byte, an offset in the low 24 bits.
    Segmented,
    /// As it is, the low 24 bits.
    Physical,
}

impl Rdram {
    pub(super) fn address(self, s: &State, w: u32) -> usize {
        match self {
            Rdram::Segmented => (s.segments[(w >> 24 & 15) as usize].wrapping_add(w & 0xff_ffff) & 0xff_ffff) as usize,
            Rdram::Physical => (w & 0xff_ffff) as usize,
        }
    }
}

/// The RSP's cycles a routine takes, fitted to the microcode's on the RSP: `fixed`, less `start` where
/// the command starts a note (flag 1), and `per` cycles each unit of its work (bytes or samples as
/// the routine counts them, rounded up to `round`, at least `least` and at most `most`). A unit is a
/// power of two, kept as its shift: the cost is worked out for every command, and a division by a
/// number known only at run time is a call on the ARM.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(super) struct Cost {
    fixed: u16,
    start: u16,
    per: u16,
    shift: u32,
    round: u16,
    least: u16,
    most: u32,
}

impl Cost {
    /// No cycles of its own (the dispatcher's take in what the routine does).
    pub(super) const NONE: Cost = Cost::fixed(0);

    pub(super) const fn fixed(fixed: u16) -> Cost {
        Cost { fixed, start: 0, per: 0, shift: 0, round: 1, least: 0, most: u32::MAX }
    }

    /// `per` cycles each `unit` of work.
    pub(super) const fn per(per: u16, unit: u16) -> Cost {
        Cost::fixed(0).and(per, unit)
    }

    pub(super) const fn and(self, per: u16, unit: u16) -> Cost {
        assert!(unit.is_power_of_two(), "a cost's unit is a power of two");
        Cost { per, shift: unit.trailing_zeros(), ..self }
    }

    /// `start` cycles less where the command starts a note.
    pub(super) const fn starting(self, start: u16) -> Cost {
        Cost { start, ..self }
    }

    pub(super) const fn rounded(self, round: u16) -> Cost {
        Cost { round, ..self }
    }

    pub(super) const fn least(self, least: u16) -> Cost {
        Cost { least, ..self }
    }

    pub(super) const fn most(self, most: u32) -> Cost {
        Cost { most, ..self }
    }

    pub(super) fn cycles(&self, work: usize, start: bool) -> u64 {
        let work = align(work, usize::from(self.round)).max(usize::from(self.least)).min(self.most as usize) as u64;
        let each = (u64::from(self.per) * work) >> self.shift;
        u64::from(self.fixed) - u64::from(start) * u64::from(self.start) + each
    }
}

/// What a routine does, with how its code reads the command.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(super) enum Routine {
    Adpcm(Adpcm),
    LoadAdpcm(LoadAdpcm),
    Resample(Resample),
    Downsample(Downsample),
    SetVol(SetVol),
    SetValue(SetValue),
    EnvSetup1(EnvSetup),
    EnvSetup2(EnvSetup),
    EnvMixer(EnvMixer),
    Mixer(Mixer),
    Interleave(Interleave),
    HiLoGain(HiLoGain),
    Polef(Polef),
    Filter(Filter),
    SetBuff(SetBuff),
    ClearBuff(ClearBuff),
    DmemMove(DmemMove),
    DmemMove2(DmemMove2),
    LoadBuff(Transfer),
    SaveBuff(Transfer),
    Segment,
    SetLoop(SetLoop),
}

use Rdram::{Physical, Segmented};

/// A note's fixed buffers, from 0x4f0: the input at 0, the dry left and right at 0x4e0 and 0x650, the wet
/// left and right at 0x7c0 and 0x930, 0x170 bytes each.
const NOTE: RampingWords = RampingWords::Note { base: 0x4f0, input: 0, dry: [0x4e0, 0x650], wet: [0x7c0, 0x930],
    count: 0x170 };

/// The routines this knows, each with the fingerprints of the code that does it.
static LIBRARY: [(&[u64], Routine); 63] = [
    // ADPCM, from SETBUFF's buffers, the codebook at 0x4c0; with the codebook at 0x3d0; packed, from the
    // fixed buffers at 0x4f0, the codebook at 0x3f0; with 2-bit frames, the codebook at 0x330.
    (&[0x60e4_de64_ddf1_8ba4], Routine::Adpcm(Adpcm { words: AdpcmWords::SetBuff, rdram: Segmented, book: 0x4c0,
        two_bit: false, aligned: false, cost: Cost::per(2, 1) })),
    (&[0x0185_69f8_dbde_72c4], Routine::Adpcm(Adpcm { words: AdpcmWords::SetBuff, rdram: Segmented, book: 0x3d0,
        two_bit: false, aligned: false, cost: Cost::per(27, 16) })),
    (&[0x70ee_a64d_02f8_d8e7, 0x973c_5802_0a8e_3f3c], Routine::Adpcm(Adpcm { words: AdpcmWords::Packed { base: 0x4f0 },
        rdram: Physical, book: 0x3f0, two_bit: false, aligned: false,
        cost: Cost::fixed(122).starting(24).and(50, 32).rounded(32) })),
    (&[0x5855_207d_2edd_41cf], Routine::Adpcm(Adpcm { words: AdpcmWords::SetBuff, rdram: Physical, book: 0x330,
        two_bit: true, aligned: true, cost: Cost::fixed(165).starting(39).and(57, 32).rounded(32) })),
    // LOADADPCM
    (&[0x2665_d735_bbb2_60d7], Routine::LoadAdpcm(LoadAdpcm { book: 0x4c0, rdram: Segmented, empty: true,
        cost: Cost::per(1, 1) })),
    (&[0xd39d_c9a0_65fa_b375], Routine::LoadAdpcm(LoadAdpcm { book: 0x3d0, rdram: Segmented, empty: true,
        cost: Cost::per(11, 16) })),
    (&[0xa78e_0a5a_38b7_7eb5, 0xedf5_1f03_620c_b925], Routine::LoadAdpcm(LoadAdpcm { book: 0x3f0, rdram: Physical,
        empty: true, cost: Cost::fixed(46) })),
    (&[0x3186_ba66_e437_7d62], Routine::LoadAdpcm(LoadAdpcm { book: 0x330, rdram: Physical, empty: false,
        cost: Cost::fixed(63) })),
    // RESAMPLE: keeping a loop's input, staged in the scratch, the filter at 0xc0; not staged, the
    // filter at 0x100; packed, keeping 10 bytes, taking VCO's carries; with modes this does not do.
    (&[0xf312_972f_bfc1_9e38], Routine::Resample(Resample { words: ResampleWords::SetBuff, rdram: Segmented,
        filter: 0xc0, loops: true, keeps_input: true, stages: true, carries: false, aligned: false,
        cost: Cost::per(5, 2) })),
    (&[0x047f_378b_53be_8c18], Routine::Resample(Resample { words: ResampleWords::SetBuff, rdram: Segmented,
        filter: 0x100, loops: true, keeps_input: true, stages: false, carries: false, aligned: false,
        cost: Cost::per(49, 16) })),
    (&[0x4c0b_b3b7_33f2_3b85, 0x60ef_c80b_3fb3_0135], Routine::Resample(Resample {
        words: ResampleWords::Packed { base: 0x4f0, outputs: [0, 0x170, 0x2e0, 0x450], count: 0x170 }, rdram: Physical,
        filter: 0xb0, loops: true, keeps_input: false, stages: false, carries: true, aligned: false,
        cost: Cost::fixed(1056).starting(22) })),
    (&[0x23f0_1e39_45f9_6438, 0xf964_9633_bf52_4410], Routine::Resample(Resample { words: ResampleWords::SetBuff, rdram: Physical,
        filter: 0xe0, loops: false, keeps_input: false, stages: false, carries: true, aligned: true,
        cost: Cost::fixed(159).starting(32).and(41, 16).rounded(16).least(16) })),
    // DOWNSAMPLE_HALF
    (&[0x9b9f_90a3_62a9_205f], Routine::Downsample(Downsample { base: 0x450, least: 0, cost: Cost::per(32, 16) })),
    (&[0x1935_bc81_7e1b_500b, 0x9b17_b0c0_ea38_9a33], Routine::Downsample(Downsample { base: 0, least: 8,
        cost: Cost::fixed(27).and(16, 8).rounded(8).least(8) })),
    // SETVOL: one value a command; three of a note's; command 14 of a build where it lands in SETVOL's
    // code on the store of the right rate's lower half.
    (&[0x6db6_0897_3b02_1dc7], Routine::SetVol(SetVol { three: false, cost: Cost::NONE })),
    (&[0x36aa_15c6_d4a6_ce81], Routine::SetVol(SetVol { three: true, cost: Cost::fixed(27) })),
    (&[0xb63b_9459_d11b_3d0d], Routine::SetValue(SetValue { value: Value::RateLow(1), cost: Cost::fixed(18) })),
    // ENVSETUP1 and ENVSETUP2: kept for ENVMIXER; left in registers for an ENVMIXER right after them.
    (&[0x5e83_1743_bdf9_f6f2], Routine::EnvSetup1(EnvSetup { registers: false, cost: Cost::NONE })),
    (&[0x4571_2eaa_4864_27c0], Routine::EnvSetup2(EnvSetup { registers: false, cost: Cost::NONE })),
    (&[0x5df4_5da9_8463_92e5], Routine::EnvSetup1(EnvSetup { registers: true, cost: Cost::fixed(29) })),
    (&[0x5c86_db39_7f54_7678], Routine::EnvSetup2(EnvSetup { registers: true, cost: Cost::fixed(29) })),
    // ENVMIXER: ramping (by multiplying, by adding; a note's, signed, and also turning inputs over where
    // a gain is odd); stepping with ENVSETUP's volumes; stepping with them in registers.
    (&[0x1603_647c_1e77_0951], Routine::EnvMixer(EnvMixer::Ramping(Ramping { words: RampingWords::SetBuff,
        rdram: Segmented, eighths: 0xb0, ramp: Ramp::Multiply, signed: false, flips: false, stages: true,
        cost: Cost::per(5, 2), falling: 0 }))),
    (&[0x75cc_048b_4cae_acef], Routine::EnvMixer(EnvMixer::Ramping(Ramping { words: RampingWords::SetBuff,
        rdram: Segmented, eighths: 0xb0, ramp: Ramp::Add, signed: false, flips: false, stages: true,
        cost: Cost::per(5, 2), falling: 0 }))),
    (&[0x51d2_1406_7127_af26], Routine::EnvMixer(EnvMixer::Ramping(Ramping { words: NOTE, rdram: Physical,
        eighths: 0xa0, ramp: Ramp::Add, signed: true, flips: false, stages: false,
        cost: Cost::fixed(813).starting(27), falling: 46 }))),
    (&[0xc54f_64c6_31fe_67b2], Routine::EnvMixer(EnvMixer::Ramping(Ramping { words: NOTE, rdram: Physical,
        eighths: 0xa0, ramp: Ramp::Add, signed: true, flips: true, stages: false,
        cost: Cost::fixed(813).starting(27), falling: 46 }))),
    (&[0x9718_9fc4_3c8d_6275], Routine::EnvMixer(EnvMixer::Stepping { base: 0x450, cost: Cost::per(24, 16) })),
    (&[0x8a50_6ae4_0b5f_aa83], Routine::EnvMixer(EnvMixer::Registers {
        cost: Cost::fixed(62).and(39, 16).rounded(16).least(16) })),
    // MIXER: SETBUFF's count, a sample at a time; w0's count; the fixed buffers, a block ahead; w0's count,
    // 32 bytes ahead.
    (&[0x3138_8b79_dd3f_19b5], Routine::Mixer(Mixer { base: 0x5c0, count: MixCount::SetBuff, order: MixOrder::Samples,
        cost: Cost::per(5, 8) })),
    (&[0x4508_9b36_5cce_d719], Routine::Mixer(Mixer { base: 0x450, count: MixCount::Units { least: 0 },
        order: MixOrder::Samples, cost: Cost::per(6, 16).rounded(32) })),
    (&[0x94bf_2230_70a8_1e8b], Routine::Mixer(Mixer { base: 0x4f0, count: MixCount::Fixed(0x170),
        order: MixOrder::Blocks, cost: Cost::fixed(221) })),
    (&[0x230a_88ce_5a25_311d, 0xf3a1_836e_1965_311d], Routine::Mixer(Mixer { base: 0, count: MixCount::Units { least: 32 },
        order: MixOrder::Rounds, cost: Cost::fixed(32).and(12, 32).rounded(32).least(32) })),
    // INTERLEAVE
    (&[0x7c09_81be_cc05_61d4], Routine::Interleave(Interleave { words: InterleaveWords::SetBuff { base: 0x5c0 },
        cost: Cost::per(2, 1) })),
    (&[0xb1f2_6651_719d_2b54], Routine::Interleave(Interleave { words: InterleaveWords::SetBuff { base: 0x450 },
        cost: Cost::per(2, 16) })),
    (&[0xfea8_f7b4_d74f_7d1d], Routine::Interleave(Interleave {
        words: InterleaveWords::Fixed { base: 0x4f0, to: 0, left: 0x4e0, right: 0x650, count: 0x170 },
        cost: Cost::fixed(573) })),
    (&[0x0cd7_d6b1_fe36_6159, 0x4024_73af_934d_14a7], Routine::Interleave(Interleave { words: InterleaveWords::Units,
        cost: Cost::fixed(27).and(16, 8).rounded(8).least(8) })),
    // HILOGAIN
    (&[0x721c_563b_40d3_a80f], Routine::HiLoGain(HiLoGain { cost: Cost::fixed(30).and(12, 32).rounded(32).least(32) })),
    // POLEF, the codebook at 0x4c0; the 8-tap filter's setup and run.
    (&[0x8a4c_0324_742d_11b4], Routine::Polef(Polef { book: 0x4c0, rdram: Segmented, cost: Cost::NONE })),
    (&[0x8be6_d551_4159_5ba7], Routine::Filter(Filter { rdram: Physical, setup: Cost::fixed(75),
        cost: Cost::fixed(138).starting(27).and(23, 16).rounded(16).least(16) })),
    // SETBUFF: with the auxiliary buffers, from 0x5c0; from 0x450; from 0.
    (&[0x469e_058d_eadb_6f51], Routine::SetBuff(SetBuff { base: 0x5c0, aux: true, cost: Cost::NONE })),
    (&[0x8ded_faa7_640f_b221], Routine::SetBuff(SetBuff { base: 0x450, aux: false, cost: Cost::NONE })),
    (&[0x6124_2496_0430_75f3], Routine::SetBuff(SetBuff { base: 0, aux: false, cost: Cost::fixed(25) })),
    // CLEARBUFF
    (&[0x18e2_527b_7659_0feb], Routine::ClearBuff(ClearBuff { base: 0x5c0, least: 0, cost: Cost::per(1, 8) })),
    (&[0x51e0_57c9_2312_4b5b], Routine::ClearBuff(ClearBuff { base: 0x450, least: 0, cost: Cost::per(4, 16) })),
    (&[0xec93_9a75_35ac_e3ef], Routine::ClearBuff(ClearBuff { base: 0x4f0, least: 16,
        cost: Cost::fixed(20).and(6, 16).rounded(16).least(16) })),
    (&[0x3934_6f97_d0c4_4389, 0x2f9b_e527_adb8_99b1], Routine::ClearBuff(ClearBuff { base: 0, least: 0,
        cost: Cost::fixed(27).and(6, 16).rounded(16) })),
    // DMEMMOVE: all read first; 16 bytes at a time.
    (&[0x7066_4bf3_9395_5147], Routine::DmemMove(DmemMove { base: 0x5c0, chunk: None, least: 0, cost: Cost::NONE })),
    (&[0x1e01_18d2_79f6_24c7], Routine::DmemMove(DmemMove { base: 0x450, chunk: Some(16), least: 0,
        cost: Cost::per(10, 16) })),
    (&[0x6943_e229_cbdc_2193], Routine::DmemMove(DmemMove { base: 0x4f0, chunk: Some(16), least: 16,
        cost: Cost::fixed(22).and(9, 16).rounded(16).least(16) })),
    (&[0x247b_feb5_d4dc_2a87, 0x7bb2_2455_4f34_62a9], Routine::DmemMove(DmemMove { base: 0, chunk: Some(16), least: 0,
        cost: Cost::fixed(27).and(9, 16).rounded(16) })),
    // DMEMMOVE2
    (&[0x2583_c58e_e576_09ff], Routine::DmemMove2(DmemMove2 { base: 0x450, once: false, aligned: false,
        cost: Cost::per(8, 16) })),
    (&[0xabb1_4c08_222c_dd9f], Routine::DmemMove2(DmemMove2 { base: 0, once: true, aligned: true,
        cost: Cost::fixed(30).and(8, 32) })),
    // LOADBUFF: SETBUFF's input and count; w0's, byte for byte; w0's 12-bit count; w0's, by DMA.
    (&[0x95cd_9e0f_aa20_528e], Routine::LoadBuff(Transfer { words: TransferWords::SetBuff, rdram: Segmented,
        empty: true, cost: Cost::NONE })),
    (&[0x4c48_80e8_cdd5_183f], Routine::LoadBuff(Transfer { words: TransferWords::Units { base: 0x450, dma: false },
        rdram: Segmented, empty: true, cost: Cost::NONE })),
    (&[0x3eb2_1f85_7132_2c7b, 0x33cd_5c14_56af_3e13], Routine::LoadBuff(Transfer {
        words: TransferWords::Bytes { base: 0x4f0 }, rdram: Physical, empty: true,
        cost: Cost::fixed(19).and(31, 1).most(1) })),
    (&[0xec8d_f631_7d4b_d240], Routine::LoadBuff(Transfer { words: TransferWords::Units { base: 0, dma: true },
        rdram: Physical, empty: false, cost: Cost::fixed(66) })),
    // SAVEBUFF
    (&[0xfbed_beee_1ca3_618c], Routine::SaveBuff(Transfer { words: TransferWords::SetBuff, rdram: Segmented,
        empty: true, cost: Cost::NONE })),
    (&[0xe996_02a7_7093_988f], Routine::SaveBuff(Transfer { words: TransferWords::Units { base: 0x450, dma: false },
        rdram: Segmented, empty: true, cost: Cost::per(7, 16) })),
    (&[0xaac4_68eb_5b86_247b, 0xc46d_180d_c2ea_fe13], Routine::SaveBuff(Transfer {
        words: TransferWords::Bytes { base: 0x4f0 }, rdram: Physical, empty: true,
        cost: Cost::fixed(19).and(31, 1).most(1) })),
    (&[0x7ef2_3859_3525_febd], Routine::SaveBuff(Transfer { words: TransferWords::Units { base: 0, dma: true },
        rdram: Physical, empty: false, cost: Cost::fixed(64) })),
    // SEGMENT
    (&[0x8617_ba0b_6d0b_16ef], Routine::Segment),
    // SETLOOP: kept with the state; kept over the table of commands at 0xe.
    (&[0x4a24_a33a_18fa_01d7], Routine::SetLoop(SetLoop { rdram: Segmented, keeps: None, cost: Cost::NONE })),
    (&[0x0785_47ed_eb9d_6421], Routine::SetLoop(SetLoop { rdram: Physical, keeps: Some(0xe), cost: Cost::fixed(21) })),
    (&[0x6bc9_4257_1df8_8837], Routine::SetLoop(SetLoop { rdram: Physical, keeps: None, cost: Cost::fixed(24) })),
];

/// The routine with this fingerprint, if this knows it.
pub(super) fn known(print: u64) -> Option<&'static Routine> {
    LIBRARY.iter().find(|(prints, _)| prints.contains(&print)).map(|(_, routine)| routine)
}

impl Routine {
    /// Whether a command of this routine runs here, by its words and the command right before it (its
    /// routine and w0) and the one before that: some take what those leave in the RSP's registers.
    pub(super) fn accepts(&self, w0: u32, w1: u32, before: Option<(&Routine, u32)>,
        two_before: Option<(&Routine, u32)>) -> bool {
        match self {
            Routine::Resample(v) => v.accepts(w0),
            Routine::LoadAdpcm(v) => v.empty || w0 & 0xffff != 0,
            Routine::LoadBuff(v) | Routine::SaveBuff(v) => v.accepts(w0),
            Routine::HiLoGain(v) => v.accepts(w1),
            Routine::DmemMove2(v) => v.accepts(w0, w1),
            Routine::Filter(v) => v.accepts(w0, before),
            // Its volumes and steps from ENVSETUP1 and ENVSETUP2 right before it.
            Routine::EnvMixer(EnvMixer::Registers { .. }) => {
                matches!(two_before, Some((Routine::EnvSetup1(EnvSetup { registers: true, .. }), _)))
                    && matches!(before, Some((Routine::EnvSetup2(EnvSetup { registers: true, .. }), _)))
            }
            _ => true,
        }
    }

    /// Whether `accepts` looks at its commands: whether some stay on the RSP for their words or the
    /// commands before them.
    pub(super) fn checked(&self) -> bool {
        match self {
            Routine::Resample(v) => !v.loops,
            Routine::LoadAdpcm(v) => !v.empty,
            Routine::LoadBuff(v) | Routine::SaveBuff(v) => !v.empty,
            Routine::DmemMove2(v) => v.aligned,
            Routine::HiLoGain(_) | Routine::Filter(_) | Routine::EnvMixer(EnvMixer::Registers { .. }) => true,
            _ => false,
        }
    }

    /// Whether it puts its state together in the scratch (s7).
    pub(super) fn stages(&self) -> bool {
        match self {
            Routine::Resample(v) => v.stages,
            Routine::EnvMixer(EnvMixer::Ramping(v)) => v.stages,
            Routine::Polef(_) => true,
            _ => false,
        }
    }

    /// Where SETLOOP's code keeps the loop's address in DMEM, if at a fixed place.
    pub(super) fn keeps_loop_at(&self) -> Option<u16> {
        match self {
            Routine::SetLoop(v) => v.keeps,
            _ => None,
        }
    }

    /// Where its code reads the resampling filter in the data.
    pub(super) fn filter(&self) -> Option<u16> {
        match self {
            Routine::Resample(v) => Some(v.filter),
            _ => None,
        }
    }

    /// Where its code reads ENVMIXER's eighths in the data.
    pub(super) fn eighths(&self) -> Option<u16> {
        match self {
            Routine::EnvMixer(EnvMixer::Ramping(v)) => Some(v.eighths),
            _ => None,
        }
    }
}

impl N64 {
    /// One command, by the routine its table entry leads to: about the RSP's cycles it takes.
    pub(super) fn command(&mut self, s: &mut State, routine: &Routine, w0: u32, w1: u32) -> u64 {
        match routine {
            Routine::Adpcm(v) => self.adpcm(s, v, w0, w1),
            Routine::LoadAdpcm(v) => self.load_adpcm(s, v, w0, w1),
            Routine::Resample(v) => self.resample(s, v, w0, w1),
            Routine::Downsample(v) => self.downsample(v, w0, w1),
            Routine::SetVol(v) => self.set_vol(s, v, w0, w1),
            Routine::SetValue(v) => self.set_value(s, v, w1),
            Routine::EnvSetup1(v) => self.env_setup1(s, v, w0, w1),
            Routine::EnvSetup2(v) => self.env_setup2(s, v, w1),
            Routine::EnvMixer(v) => self.env_mixer(s, v, w0, w1),
            Routine::Mixer(v) => self.mixer(s, v, w0, w1),
            Routine::Interleave(v) => self.interleave(s, v, w0, w1),
            Routine::HiLoGain(v) => self.hilogain(v, w0, w1),
            Routine::Polef(v) => self.polef(s, v, w0, w1),
            Routine::Filter(v) => self.filter(s, v, w0, w1),
            Routine::SetBuff(v) => self.set_buff(s, v, w0, w1),
            Routine::ClearBuff(v) => self.clear_buff(v, w0, w1),
            Routine::DmemMove(v) => self.dmem_move_command(v, w0, w1),
            Routine::DmemMove2(v) => self.dmem_move2(v, w0, w1),
            Routine::LoadBuff(v) => self.load_buff(s, v, w0, w1),
            Routine::SaveBuff(v) => self.save_buff(s, v, w0, w1),
            Routine::Segment => {
                s.segments[(w1 >> 24 & 15) as usize] = w1 & 0xff_ffff;
                0
            }
            Routine::SetLoop(v) => self.set_loop(s, v, w1),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routines_not_checked_take_any_command() {
        // What a task's check looks no further at: any words, after any routine or none.
        let words = [0, 1, 0x0001_0000, 0x0002_0000, 0x0006_0000, 0x0f0f_0f0f, 0x00ff_ffff, 0xffff_ffff];
        for (_, routine) in LIBRARY.iter().filter(|(_, routine)| !routine.checked()) {
            for (&w0, &w1) in words.iter().flat_map(|w0| words.iter().map(move |w1| (w0, w1))) {
                for before in [None, Some((routine, w0)), Some((&LIBRARY[0].1, w1))] {
                    assert!(routine.accepts(w0, w1, before, before), "{routine:?} {w0:08x} {w1:08x}");
                }
            }
        }
    }

    #[test]
    fn a_fingerprint_is_one_routine() {
        let mut prints: Vec<u64> = LIBRARY.iter().flat_map(|(prints, _)| prints.iter().copied()).collect();
        let count = prints.len();
        prints.sort_unstable();
        prints.dedup();
        assert_eq!(prints.len(), count);
    }
}
