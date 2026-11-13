//! COP0, the VR4300's system control coprocessor: its registers, the
//! TLB, and the translation of virtual addresses to physical ones.
//!
//! Translation, in the modes the Status register sets (kernel when EXL or
//! ERL is set, or KSU is 0; then supervisor and user), and with 32-bit
//! addresses (sign-extended) unless KX, SX or UX gives the mode 64-bit
//! ones:
//!
//! | Virtual (32-bit)      | What                                       |
//! |-----------------------|--------------------------------------------|
//! | 0000_0000-7FFF_FFFF   | kuseg: through the TLB (unmapped with ERL) |
//! | 8000_0000-9FFF_FFFF   | kseg0: physical 0, cached (kernel only)    |
//! | A000_0000-BFFF_FFFF   | kseg1: physical 0, uncached (kernel only)  |
//! | C000_0000-DFFF_FFFF   | ksseg: through the TLB (not in user mode)  |
//! | E000_0000-FFFF_FFFF   | kseg3: through the TLB (kernel only)       |
//!
//! The 64-bit space adds xkphys (8000_0000_0000_0000-BFFF_FFFF_FFFF_FFFF,
//! physical, kernel only) and makes the mapped segments 2^40 bytes long
//! (xkseg 2 GB less). A TLB miss goes to the refill handler of the mode's
//! address size (KX, SX or UX), whatever the address's.
//! Caches are not emulated: cached and uncached segments behave the same.

use savestate::state;

// Status.
pub const IE: u32 = 1 << 0;
pub const EXL: u32 = 1 << 1;
pub const ERL: u32 = 1 << 2;
const UX: u32 = 1 << 5;
const SX: u32 = 1 << 6;
const KX: u32 = 1 << 7;
const BEV: u32 = 1 << 22;
pub const FR: u32 = 1 << 26;
pub const CU0: u32 = 1 << 28;
pub const CU1: u32 = 1 << 29;
pub const CU2: u32 = 1 << 30;

// Cause: the branch delay bit; the interrupt bits are its 8-15.
pub const BD: u32 = 1 << 31;
/// The RCP's interrupt line (the MIPS interface's) and the timer's.
pub const IP2: u32 = 1 << 10;
pub const IP7: u32 = 1 << 15;

/// What a register reads as whatever was written to it: PRId, and CacheErr and TagHi, which hold
/// nothing here. The interpreter and the A64 compiler both read them from here.
pub fn constant_register(reg: usize) -> Option<u64> {
    match reg {
        15 => Some(0x0b22),
        27 | 29 => Some(0),
        _ => None,
    }
}

/// The kinds of access, for translation and its exceptions.
#[derive(Clone, Copy, PartialEq)]
pub enum Access {
    Fetch,
    Load,
    Store,
}

/// Why an address did not translate.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Fault {
    /// Misaligned, or out of the mode's reach.
    Address,
    /// No TLB entry matches.
    TlbMiss,
    /// The entry's page is not valid.
    TlbInvalid,
    /// A store to a page not marked dirty (writable).
    TlbModified,
}

/// A TLB entry: an even and odd page of a size, at a virtual page pair.
#[derive(Clone, Copy, Default)]
pub struct TlbEntry {
    page_mask: u64,
    /// EntryHi: the region (bits 62-63), the VPN2 and the ASID.
    entry_hi: u64,
    /// EntryLo0 and 1 without their G bits: the page frame, cache
    /// attribute, dirty and valid bits.
    lo: [u64; 2],
    global: bool,
}

state!(TlbEntry { page_mask, entry_hi, lo, global });

impl TlbEntry {
    /// An entry that never matches: for a page pair of kseg0, which is not
    /// mapped. The TLB holds leftovers at power on; this is how libultra's
    /// osUnmapTLBAll leaves it, and an entry of zeros would match address 0
    /// instead.
    fn unmapped(i: usize) -> TlbEntry {
        TlbEntry { entry_hi: 0xffff_ffff_8000_0000 | (i as u64) << 13, ..TlbEntry::default() }
    }
}

/// The TLB's entries, and what tlb_lookup found in them by 4 KB virtual
/// page and ASID: the physical page, and whether it may be written. Those
/// are cleared whenever an entry changes or a state loads (they are no
/// part of it): a lookup gives what a search of the entries would.
#[derive(Clone)]
pub struct Tlb {
    pub entries: [TlbEntry; 32],
    pages: [std::cell::Cell<(u64, u32)>; 64],
    /// Counts the times the pages were forgotten (the TLB changed): what
    /// a translation kept elsewhere is checked against.
    pub(crate) epoch: std::cell::Cell<u32>,
}

const NO_PAGE: (u64, u32) = (u64::MAX, 0);

impl Tlb {
    fn new() -> Tlb {
        Tlb {
            entries: std::array::from_fn(TlbEntry::unmapped),
            pages: std::array::from_fn(|_| NO_PAGE.into()),
            epoch: std::cell::Cell::new(0),
        }
    }

    fn forget_pages(&self) {
        for page in &self.pages {
            page.set(NO_PAGE);
        }
        self.epoch.set(self.epoch.get().wrapping_add(1));
    }
}

impl savestate::State for Tlb {
    fn save(&self, out: &mut Vec<u8>) {
        self.entries.save(out);
    }

    fn load(&mut self, input: &mut &[u8]) -> Result<(), savestate::BadState> {
        self.forget_pages();
        self.entries.load(input)
    }
}

#[derive(Clone)]
pub struct Cop0 {
    pub index: u32,
    pub entry_lo: [u64; 2],
    pub context: u64,
    pub page_mask: u64,
    pub wired: u32,
    pub bad_vaddr: u64,
    pub entry_hi: u64,
    pub compare: u32,
    pub status: u32,
    pub cause: u32,
    pub epc: u64,
    pub config: u32,
    pub ll_addr: u32,
    pub watch_lo: u32,
    pub watch_hi: u32,
    pub x_context: u64,
    pub parity_error: u32,
    pub tag_lo: u32,
    pub error_epc: u64,
    /// The last value moved to COP0, which the registers it does not have
    /// read back.
    pub latch: u64,
    /// Count, as it was at a cycle; it goes up by one every two cycles.
    pub count: u32,
    pub count_cycle: u64,
    /// The cycle Random was last set to 31 at, from which it counts down.
    pub random_cycle: u64,
    pub tlb: Tlb,
}

state!(Cop0 { index, entry_lo, context, page_mask, wired, bad_vaddr, entry_hi, compare, status, cause, epc, config,
    ll_addr, watch_lo, watch_hi, x_context, parity_error, tag_lo, error_epc, latch, count, count_cycle, random_cycle,
    tlb });

impl Cop0 {
    pub fn new() -> Cop0 {
        Cop0 {
            index: 0,
            entry_lo: [0; 2],
            context: 0,
            page_mask: 0,
            wired: 0,
            bad_vaddr: 0,
            entry_hi: 0,
            compare: 0,
            status: 0,
            cause: 0,
            epc: 0,
            config: 0x7006_e463,
            ll_addr: 0,
            watch_lo: 0,
            watch_hi: 0,
            x_context: 0,
            parity_error: 0,
            tag_lo: 0,
            error_epc: 0,
            latch: 0,
            count: 0,
            count_cycle: 0,
            random_cycle: 0,
            tlb: Tlb::new(),
        }
    }

    pub fn set_ip(&mut self, ip: u32, on: bool) {
        if on {
            self.cause |= ip;
        } else {
            self.cause &= !ip;
        }
    }

    /// Whether an interrupt is pending, enabled, and let through (IE set,
    /// EXL and ERL clear).
    #[inline]
    pub fn interrupt_pending(&self) -> bool {
        self.cause & self.status & 0xff00 != 0 && self.status & (IE | EXL | ERL) == IE
    }

    pub fn kernel(&self) -> bool {
        self.status & (EXL | ERL) != 0 || self.status & 0x18 == 0
    }

    /// Whether the mode has 64-bit addresses: KX in kernel mode, SX in
    /// supervisor mode (KSU 1), UX in user mode (KSU 2, and 3).
    pub fn wide(&self) -> bool {
        let status = self.status;
        let bit = if self.kernel() { KX } else if status & 0x18 == 0x08 { SX } else { UX };
        status & bit != 0
    }

    /// Whether the 64-bit integer instructions may run: always in kernel
    /// mode, and in supervisor and user mode only with 64-bit addresses (SX,
    /// UX). Otherwise they are reserved instructions (n64-systemtest's
    /// "64-bit instructions reserved in non-kernel mode").
    pub fn allows_64_bit(&self) -> bool {
        self.kernel() || self.wide()
    }

    /// The vector an exception goes to: past the boot ROM's area while BEV
    /// is set, and at the start of kseg0 otherwise.
    pub fn vector(&self, offset: u64) -> u64 {
        let base = if self.status & BEV != 0 { 0xffff_ffff_bfc0_0200 } else { 0xffff_ffff_8000_0000 };
        base + offset
    }

    /// Random: a six-bit counter that goes down by one an instruction from
    /// 31 (where the Wired write sets it) and starts over at 31 when it
    /// equals Wired. With Wired up to 31 it runs from 31 to Wired; above 31
    /// it passes 0 and wraps to 63 before it meets Wired, so it takes values
    /// from the whole range (n64-systemtest's "Wired OOB/Random").
    pub fn random(&self, cycles: u64) -> u32 {
        let span = u64::from((31u32.wrapping_sub(self.wired) & 63) + 1);
        31u32.wrapping_sub(((cycles - self.random_cycle) % span) as u32) & 63
    }

    /// Translates a virtual address of the kseg0 and kseg1 segments, which
    /// most code runs in, without the rest of translate's checks.
    #[inline]
    pub fn translate_fast(&self, vaddr: u64) -> Option<u32> {
        if vaddr.wrapping_sub(0xffff_ffff_8000_0000) < 0x4000_0000 && self.kernel() {
            Some(vaddr as u32 & 0x1fff_ffff)
        } else {
            None
        }
    }

    pub fn translate(&self, vaddr: u64, access: Access) -> Result<u32, Fault> {
        if let Some(phys) = self.translate_fast(vaddr) {
            return Ok(phys);
        }
        let status = self.status;
        let (kernel, supervisor) = (self.kernel(), !self.kernel() && status & 0x18 == 0x08);
        let wide = status & if kernel { KX } else if supervisor { SX } else { UX } != 0;
        let low = vaddr as u32;
        if !wide || vaddr >= 0xffff_ffff_8000_0000 {
            // 32-bit addresses, which must be sign-extended; and the 64-bit
            // space's last 2 GB, which are the same segments.
            if !wide && vaddr != low as i32 as i64 as u64 {
                return Err(Fault::Address);
            }
            return match low >> 29 {
                0..=3 if kernel && status & ERL != 0 => Ok(low),
                0..=3 => self.tlb_lookup(vaddr, access),
                4 | 5 if kernel => Ok(low & 0x1fff_ffff),
                6 if kernel || supervisor => self.tlb_lookup(vaddr, access),
                7 if kernel => self.tlb_lookup(vaddr, access),
                _ => Err(Fault::Address),
            };
        }
        // The 64-bit segments of 2^40 bytes (xkseg's last 2 GB are not
        // there), and xkphys between them.
        let offset = vaddr & 0x3fff_ffff_ffff_ffff;
        match vaddr >> 62 {
            0 if offset < 1 << 40 => self.tlb_lookup(vaddr, access),
            1 if offset < 1 << 40 && (kernel || supervisor) => self.tlb_lookup(vaddr, access),
            2 if kernel && vaddr & 0x07ff_ffff_0000_0000 == 0 => Ok(low),
            3 if offset < (1 << 40) - (1 << 31) && kernel => self.tlb_lookup(vaddr, access),
            _ => Err(Fault::Address),
        }
    }

    fn tlb_lookup(&self, vaddr: u64, access: Access) -> Result<u32, Fault> {
        let key = (vaddr >> 12) << 8 | self.entry_hi & 0xff;
        let cached = &self.tlb.pages[(vaddr >> 12) as usize & 63];
        let (tag, page) = cached.get();
        if tag == key && (access != Access::Store || page & 1 != 0) {
            return Ok(page & !0xfff | vaddr as u32 & 0xfff);
        }
        let (physical, writable) = self.tlb_search(vaddr, access)?;
        cached.set((key, physical & !0xfff | u32::from(writable)));
        Ok(physical)
    }

    /// The first entry that maps the address: the physical address, and
    /// whether the page may be written.
    fn tlb_search(&self, vaddr: u64, access: Access) -> Result<(u32, bool), Fault> {
        let asid = self.entry_hi & 0xff;
        for entry in &self.tlb.entries {
            // The VPN2 bits the page size leaves, and the region.
            let mask = !(entry.page_mask | 0x1fff) & 0xc000_00ff_ffff_e000;
            if (vaddr ^ entry.entry_hi) & mask != 0 || !entry.global && entry.entry_hi & 0xff != asid {
                continue;
            }
            let size = (entry.page_mask >> 1 | 0xfff) + 1;
            let lo = entry.lo[usize::from(vaddr & size != 0)];
            if lo & 2 == 0 {
                return Err(Fault::TlbInvalid);
            }
            if access == Access::Store && lo & 4 == 0 {
                return Err(Fault::TlbModified);
            }
            let frame = (lo >> 6 & 0xf_ffff) << 12;
            return Ok(((frame | vaddr & (size - 1)) as u32, lo & 4 != 0));
        }
        Err(Fault::TlbMiss)
    }

    /// What an address error or a TLB exception leaves for the handler: the
    /// address, and its VPN2 in Context, XContext and EntryHi (with the
    /// region in the last two; the ASID stays). A console fills them in for
    /// address errors too (krom's exception tests read Context, n64-systemtest
    /// EntryHi after misaligned loads and ones out of the 64-bit segments).
    pub fn bad_address(&mut self, vaddr: u64) {
        self.bad_vaddr = vaddr;
        self.context = self.context & !0x7f_fff0 | (vaddr >> 9) & 0x7f_fff0;
        self.x_context = self.x_context & !0x1_ffff_fff0 | (vaddr >> 31 & 0x1_8000_0000) | (vaddr >> 9 & 0x7fff_fff0);
        self.entry_hi = self.entry_hi & 0xff | vaddr & 0xc000_00ff_ffff_e000;
    }

    /// TLBR: an entry back into PageMask, EntryHi and EntryLo0 and 1.
    pub fn tlb_read(&mut self) {
        let entry = self.tlb.entries[self.index as usize & 31];
        self.page_mask = entry.page_mask;
        self.entry_hi = entry.entry_hi & !entry.page_mask;
        self.entry_lo = entry.lo.map(|lo| lo | u64::from(entry.global));
    }

    /// TLBWI and TLBWR: PageMask, EntryHi and EntryLo0 and 1 into an
    /// entry, which is global only if both EntryLo registers say so. The
    /// entry keeps less than the registers hold (n64-systemtest reads it
    /// back with TLBR): of each EntryLo the page frame, cache attribute,
    /// dirty and valid bits (1-25, not 26-29), and of PageMask's six pairs
    /// of bits each pair's upper bit, read back as both (0x2000 as 0, 0x4000
    /// as 0x6000); EntryHi's VPN2 loses the bits that mask covers.
    pub fn tlb_write(&mut self, i: usize) {
        let upper = self.page_mask & 0x0155_4000;
        let page_mask = upper | upper >> 1;
        self.tlb.entries[i & 31] = TlbEntry {
            page_mask,
            entry_hi: self.entry_hi & !page_mask,
            lo: self.entry_lo.map(|lo| lo & 0x03ff_fffe),
            global: self.entry_lo[0] & self.entry_lo[1] & 1 != 0,
        };
        self.tlb.forget_pages();
    }

    /// TLBP: the entry that matches EntryHi in Index, or the probe failure
    /// bit.
    pub fn tlb_probe(&mut self) {
        let asid = self.entry_hi & 0xff;
        let found = self.tlb.entries.iter().position(|entry| {
            let mask = !(entry.page_mask | 0x1fff) & 0xc000_00ff_ffff_e000;
            (self.entry_hi ^ entry.entry_hi) & mask == 0 && (entry.global || entry.entry_hi & 0xff == asid)
        });
        self.index = match found {
            Some(i) => i as u32,
            None => 1 << 31,
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use savestate::State;

    fn random(seed: &mut u64) -> u64 {
        *seed ^= *seed << 13;
        *seed ^= *seed >> 7;
        *seed ^= *seed << 17;
        *seed
    }

    /// Random from the Wired write on: down from 31 to Wired and over again,
    /// or, with Wired above 31, through 0 and 63 down to Wired.
    #[test]
    fn random_counts_down_to_wired_through_all_six_bits() {
        let mut cop0 = Cop0::new();
        for (wired, expected) in [
            (0, (0..32).rev().collect::<Vec<u32>>()),
            (5, (5..32).rev().collect()),
            (31, vec![31]),
            (32, (0..32).rev().chain((32..64).rev()).collect()),
            (40, (0..32).rev().chain((40..64).rev()).collect()),
            (63, (0..32).rev().chain([63]).collect()),
        ] {
            cop0.wired = wired;
            cop0.random_cycle = 1000;
            let seen: Vec<u32> = (0..2 * expected.len() as u64).map(|n| cop0.random(1000 + n)).collect();
            let twice: Vec<u32> = expected.iter().chain(&expected).copied().collect();
            assert_eq!(seen, twice, "Wired {wired}");
        }
    }

    /// TLBWI then TLBR gives back what an entry keeps: EntryLo's bits 1-25
    /// and the G both had, PageMask's pairs by their upper bits, EntryHi's
    /// region, VPN2 outside the mask and ASID (n64-systemtest's "PageMask
    /// after TLBWI/TLBR" and "EntryLo0/1/Hi after TLBWI/TLBR").
    #[test]
    fn an_entry_keeps_what_tlbr_reads_back() {
        let mut cop0 = Cop0::new();
        for (written, kept) in [
            (0, 0), (0x2000, 0), (0x4000, 0x6000), (0x6000, 0x6000), (0x1_e000, 0x1_e000), (0x8000, 0),
            (0x1_0000, 0x1_8000), (0xa000, 0), (0x01ff_e000, 0x01ff_e000), (0x0100_0000, 0x0180_0000),
            (0x0155_4000, 0x01ff_e000), (0x00aa_a000, 0),
        ] {
            cop0.page_mask = written;
            cop0.entry_hi = 0xc000_00ff_ffff_e0ff;
            cop0.entry_lo = [0x3fff_ffff, 0x0123_4566];
            cop0.tlb_write(6);
            cop0.page_mask = 0x1234_5678;
            cop0.index = 6;
            cop0.tlb_read();
            assert_eq!(cop0.page_mask, kept, "PageMask {written:x}");
            assert_eq!(cop0.entry_hi, 0xc000_00ff_ffff_e0ff & !kept, "EntryHi with PageMask {written:x}");
            assert_eq!(cop0.entry_lo, [0x03ff_fffe, 0x0123_4566]);
        }
        cop0.entry_lo = [0x3fff_ffff, 1];
        cop0.tlb_write(7);
        cop0.index = 7;
        cop0.tlb_read();
        assert_eq!(cop0.entry_lo, [0x03ff_ffff, 1]);
    }

    /// Random TLBs (sizes, global and ASID pages, valid and dirty bits),
    /// rewritten as it goes, ASIDs changed and a state loaded: every lookup
    /// through the page cache gives what a search of the entries gives.
    #[test]
    fn cached_lookups_match_searching_the_entries() {
        let mut seed = 0x71b0_ca5e_2026_1030;
        let mut cop0 = Cop0::new();
        let masks = [0, 0x6000, 0x1_e000, 0x7_e000, 0x1f_e000, 0x7f_e000, 0x1ff_e000];
        let write = |cop0: &mut Cop0, seed: &mut u64, i: usize| {
            let r = random(seed);
            cop0.page_mask = masks[r as usize % masks.len()];
            // Few distinct pages, so entries and lookups meet.
            cop0.entry_hi = (r >> 8 & 0x7) << 13 | (r >> 12 & 1) << 30 | r >> 16 & 3;
            cop0.entry_lo = [random(seed) & 0x3fff_ffff, random(seed) & 0x3fff_ffff];
            cop0.tlb_write(i);
        };
        for i in 0..32 {
            write(&mut cop0, &mut seed, i);
        }
        let mut hits = 0;
        for step in 0..200_000 {
            let r = random(&mut seed);
            match r % 64 {
                0 => write(&mut cop0, &mut seed, (r >> 8) as usize % 32),
                1 => cop0.entry_hi = cop0.entry_hi & !0xff | r >> 8 & 3,
                2 => {
                    let mut saved = Vec::new();
                    cop0.save(&mut saved);
                    let mut other = Cop0::new();
                    write(&mut other, &mut seed, 0);
                    // Loading over a cache that saw other entries.
                    for page in 0..64u64 {
                        let _ = other.tlb_lookup(page << 12, Access::Load);
                    }
                    other.load(&mut saved.as_slice()).unwrap();
                    cop0 = other;
                }
                _ => {}
            }
            let vaddr = (r >> 16 & 0x7) << 13 | (r >> 20 & 1) << 30 | r >> 24 & 0x1fff;
            let access = [Access::Fetch, Access::Load, Access::Store][(r >> 40) as usize % 3];
            let expected = cop0.tlb_search(vaddr, access).map(|(physical, _)| physical);
            let (tag, _) = cop0.tlb.pages[(vaddr >> 12) as usize & 63].get();
            hits += u32::from(tag == (vaddr >> 12) << 8 | cop0.entry_hi & 0xff);
            assert_eq!(cop0.tlb_lookup(vaddr, access), expected, "step {step}, {vaddr:x}");
        }
        assert!(hits > 20_000, "only {hits} lookups found their page cached");
    }
}
