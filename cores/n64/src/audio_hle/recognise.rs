//! Knowing a sound microcode by its own code. Its dispatcher, the loop the routines go back to for the
//! next command, is where the code's jumps go most; the dispatcher's fingerprint says where its table
//! of commands is and how many it has, the data its routines read, and its own share of the RSP's
//! time. Each command is the routine its table entry leads to, known by its fingerprint in the library
//! (routines.rs); an entry leading back to the dispatcher does nothing. The routines' code also says
//! where the resampling filter and ENVMIXER's eighths are in the data, and the start code where the
//! scratch is (s7). A task with a command whose routine is not known, or not with what it needs before
//! it, stays on the RSP.

use super::routines::{known, Routine};
use crate::N64;

/// Where a task's microcode lies in IMEM: a task whose boot microcode is the microcode itself
/// (ucode_boot the same as ucode) has all 4 KB of it from 0x1000; otherwise the boot microcode loads
/// 0xf80 bytes of it from 0x1080 and keeps 0x1000 to 0x107f.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(super) struct Code {
    /// The microcode in RDRAM, and the IMEM address its first byte goes to.
    text: usize,
    from: u32,
}

impl Code {
    /// The running task's.
    pub(super) fn of(n64: &N64) -> Code {
        let task = |o: u32| n64.rsp.read_memory(0xfc0 + o) & 0x7f_ffff;
        Code { text: task(0x10) as usize, from: if task(0x08) == task(0x10) { 0x1000 } else { 0x1080 } }
    }

    /// The instruction at an IMEM address, if the microcode has one there.
    pub(super) fn word(&self, rdram: &[u8], imem: u32) -> Option<u32> {
        let at = self.text + imem.checked_sub(self.from).filter(|&o| o + 4 <= 0x2000 - self.from)? as usize;
        Some(u32::from_be_bytes(rdram.get(at..at + 4)?.try_into().unwrap()))
    }
}

/// A routine's fingerprint (FNV-1a): its instructions and those of the code it reaches by jumps,
/// calls and branches (but the dispatcher), in the order they are reached, each without its jump
/// target or branch offset; a run of code ends with a jump or a return (and its delay slot).
pub(crate) fn fingerprint(word: &dyn Fn(u32) -> Option<u32>, entry: u32, dispatcher: u32) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    let (mut seen, mut stack, mut count) = (Vec::new(), vec![entry], 0);
    while let Some(start) = stack.pop() {
        if start == dispatcher || seen.contains(&start) {
            continue;
        }
        seen.push(start);
        let mut pc = start;
        let mut ending = false;
        while let Some(w) = word(pc) {
            count += 1;
            if count > 4096 {
                return hash;
            }
            let op = w >> 26;
            let masked = match op {
                2 | 3 => {
                    stack.push(0x1000 | w << 2 & 0xffc);
                    w & 0xfc00_0000
                }
                1 | 4..=7 => {
                    stack.push(0x1000 | (pc + 4).wrapping_add((w as i16 as i32 as u32) << 2) & 0xffc);
                    w & 0xffff_0000
                }
                _ => w,
            };
            hash = (hash ^ u64::from(masked)).wrapping_mul(0x100_0000_01b3);
            if ending {
                break;
            }
            ending = op == 2 || op == 0 && w & 0x3f == 8;
            pc += 4;
        }
    }
    hash
}

/// The dispatcher: where the code's jumps (J) go most (the last of the commonest).
fn dispatcher(word: &dyn Fn(u32) -> Option<u32>) -> u32 {
    let mut targets = std::collections::BTreeMap::new();
    for pc in (0x1000..0x2000).step_by(4) {
        if let Some(w) = word(pc) && w >> 26 == 2 {
            *targets.entry(0x1000 | w << 2 & 0xffc).or_insert(0) += 1;
        }
    }
    targets.into_iter().max_by_key(|&(_, n)| n).map_or(0, |(at, _)| at)
}

/// What the start code sets a register to (ADDI, ADDIU or ORI from zero) before its first jump, branch
/// or break (calls go on after their return).
fn start_value(word: &dyn Fn(u32) -> Option<u32>, from: u32, register: u32) -> Option<usize> {
    let mut pc = from;
    while let Some(w) = word(pc) {
        let op = w >> 26;
        if matches!(op, 8 | 9 | 13) && w >> 21 & 31 == 0 && w >> 16 & 31 == register {
            return Some(usize::from(w as u16));
        }
        if matches!(op, 1 | 2 | 4..=7) || op == 0 && matches!(w & 0x3f, 8 | 9 | 13) {
            return None;
        }
        pc += 4;
    }
    None
}

/// Whether a word stored at `keeps` covers entry `i` of a table of commands at `table`.
fn covers(keeps: usize, table: usize, i: usize) -> bool {
    table + 2 * i + 2 > keeps && table + 2 * i < keeps + 4
}

/// A dispatcher: its table of commands, the data its routines read, and its share of the RSP's time.
#[derive(Debug)]
pub(super) struct Dispatcher {
    /// Where its LH finds the table of commands in DMEM (where the data puts it), and how many commands
    /// the table has.
    table: u16,
    commands: u8,
    /// The data's size at least, and the constants its routines read from it, where they read them.
    size: u16,
    data: &'static [(u16, &'static [u16])],
    /// The RSP's cycles of its own: a task, each command, and a command whose entry is the dispatcher
    /// itself (None: lists use none, so its time is not known, and a task with one stays on the RSP).
    pub task: u16,
    pub command: u16,
    pub idle: Option<u16>,
    /// Fetching the command list: `cost` cycles before command n where n % `every` is `at`.
    pub fetch: Option<Fetch>,
    /// Whether the VCO and VCC its routines leave go on to the next task (whose first VSUB takes VCO's
    /// carries): kept as the RSP has them.
    pub flags: bool,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct Fetch {
    pub cost: u16,
    pub every: u16,
    pub at: u16,
}

/// The constants the routines of the dispatchers with a table of 16 or 24 commands read after it: the
/// ADPCM masks and multipliers, the resampler's lane offsets and ones, the lane selectors, rows of one
/// lane each, and ENVMIXER's eighths.
const CONSTANTS: [u16; 72] = [
    0xf000, 0x0f00, 0x00f0, 0x000f, 0x0001, 0x0010, 0x0100, 0x1000, 0x0002, 0x0004, 0x0006, 0x0008, 0x000a, 0x000c,
    0x000e, 0x0010, 0x0001, 0x0001, 0x0001, 0x0001, 0x0001, 0x0001, 0x0001, 0x0001, 0x0000, 0x0001, 0x0002, 0x0004,
    0x0008, 0x0010, 0x0100, 0x0200, 0x0001, 0x0000, 0x0000, 0x0000, 0x0001, 0x0000, 0x0000, 0x0000, 0x0000, 0x0001,
    0x0000, 0x0000, 0x0000, 0x0001, 0x0000, 0x0000, 0x0000, 0x0000, 0x0001, 0x0000, 0x0000, 0x0000, 0x0001, 0x0000,
    0x0000, 0x0000, 0x0000, 0x0001, 0x0000, 0x0000, 0x0000, 0x0001, 0x2000, 0x4000, 0x6000, 0x8000, 0xa000, 0xc000,
    0xe000, 0xffff,
];

/// The masks and multipliers of 2-bit ADPCM frames.
const TWO_BIT: [u16; 8] = [0xc000, 0x3000, 0x0c00, 0x0300, 0x0001, 0x0004, 0x0010, 0x0040];

/// The dispatchers this knows, by the fingerprints of their loop (from where the routines go back to,
/// the code it reaches: the command's fetch, the list's and the task's end). The costs were fitted with
/// the routines' to the microcode's time on the RSP.
static DISPATCHERS: [(&[u64], Dispatcher); 4] = [
    (&[0x53d4_d6d8_59b6_1aa0], Dispatcher { table: 0x10, commands: 16, size: 0x2c0,
        data: &[(0, &[0, 1]), (0x30, &CONSTANTS)], task: 0, command: 52, idle: Some(0), fetch: None, flags: false }),
    (&[0x2042_60a5_4103_40a0], Dispatcher { table: 0x10, commands: 24, size: 0x300,
        data: &[(0, &[0, 1, 2, 0xffff]), (0x50, &CONSTANTS)], task: 0, command: 64, idle: Some(0), fetch: None,
        flags: false }),
    (&[0xcb1b_2518_4499_c184, 0x5735_a562_d929_1282], Dispatcher { table: 0, commands: 16, size: 0x2b0,
        data: &[(0x20, &[0xf000, 0x0f00, 0x00f0, 0x000f]), (0xa0, &[0x2000]), (0xae, &[0xffff, 0x0c39, 0x66ad])],
        task: 130, command: 0, idle: Some(20), fetch: Some(Fetch { cost: 42, every: 40, at: 0 }), flags: true }),
    (&[0x8eec_3bec_3df7_3831], Dispatcher { table: 0x10, commands: 24, size: 0x2df,
        data: &[(0, &[0, 1, 2, 0xffff, 0x20, 0x800, 0x7fff, 0x4000]), (0x40, CONSTANTS.split_at(8).0),
            (0x50, &TWO_BIT), (0x60, CONSTANTS.split_at(64).0.split_at(8).1)],
        task: 109, command: 0, idle: None, fetch: Some(Fetch { cost: 52, every: 8, at: 7 }), flags: true }),
];

/// What a command's table entry leads to.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(super) enum Entry {
    /// The dispatcher: the command does nothing.
    Idle,
    Runs(&'static Routine),
}

/// A task's microcode, as its code and data describe it.
#[derive(Clone, Copy, Debug)]
pub(super) struct Microcode {
    pub dispatcher: &'static Dispatcher,
    /// What each command does, where this knows it.
    commands: [Option<Entry>; 32],
    /// A bit for each command this knows, and for each whose routine runs here only with some words or
    /// some commands before it (`Routine::accepts`): the list's check looks no further at the rest.
    known: u32,
    checked: u32,
    /// The scratch the start code points s7 at, where routines put their states together.
    pub scratch: Option<usize>,
    /// Where RESAMPLE's code reads the resampling filter in the data, and ENVMIXER's its eighths.
    pub filter: Option<u16>,
    pub eighths: Option<u16>,
}

impl Microcode {
    /// The microcode with this data (`size` bytes at `data`) and code, if this knows its dispatcher, the
    /// data has what its routines read, and it has a routine this knows.
    pub(super) fn of(rdram: &[u8], data: usize, size: u32, code: Code) -> Option<Microcode> {
        let word = |imem: u32| code.word(rdram, imem);
        let at = dispatcher(&word);
        let print = fingerprint(&word, at, 0);
        let (_, dispatcher) = DISPATCHERS.iter().find(|(prints, _)| prints.contains(&print))?;
        let half = |o: usize| rdram.get(data + o..data + o + 2).map(|b| u16::from_be_bytes([b[0], b[1]]));
        let has = |o: u16, values: &[u16]| {
            values.iter().enumerate().all(|(i, &v)| half(usize::from(o) + 2 * i) == Some(v))
        };
        if size < u32::from(dispatcher.size) || !dispatcher.data.iter().all(|&(o, values)| has(o, values)) {
            return None;
        }
        let scratch = start_value(&word, code.from, 23);
        // The RSP's PC has 12 bits, so an entry leads to where it lands in IMEM.
        let table = usize::from(dispatcher.table);
        let mut commands = [None; 32];
        for (i, command) in commands.iter_mut().enumerate().take(usize::from(dispatcher.commands)) {
            let Some(entry) = half(table + 2 * i).map(|h| 0x1000 | u32::from(h) & 0xffc) else { continue };
            *command = if entry == at {
                dispatcher.idle.map(|_| Entry::Idle)
            } else if word(entry).is_some() {
                known(fingerprint(&word, entry, at)).filter(|r| scratch.is_some() || !r.stages()).map(Entry::Runs)
            } else {
                None
            };
        }
        // A SETLOOP that keeps the loop's address over the table: the entries it covers lead elsewhere.
        let keeps: Vec<u16> = commands.iter().filter_map(|c| match c {
            Some(Entry::Runs(routine)) => routine.keeps_loop_at(),
            _ => None,
        }).collect();
        for keeps in keeps {
            for (i, command) in commands.iter_mut().enumerate() {
                if covers(usize::from(keeps), table, i) {
                    *command = None;
                }
            }
        }
        Microcode::new(dispatcher, commands, scratch)
    }

    /// A microcode with these commands, if it has a routine: where its routines read the data.
    fn new(dispatcher: &'static Dispatcher, commands: [Option<Entry>; 32], scratch: Option<usize>)
        -> Option<Microcode> {
        let routines = || commands.iter().filter_map(|c| match c {
            Some(Entry::Runs(routine)) => Some(*routine),
            _ => None,
        });
        routines().next()?;
        let bits = |which: &dyn Fn(&Entry) -> bool| {
            commands.iter().enumerate().filter(|(_, c)| c.as_ref().is_some_and(which)).fold(0, |b, (i, _)| b | 1 << i)
        };
        Some(Microcode {
            dispatcher,
            commands,
            known: bits(&|_| true),
            checked: bits(&|c| matches!(c, Entry::Runs(routine) if routine.checked())),
            scratch,
            filter: routines().find_map(|r| r.filter()),
            eighths: routines().find_map(|r| r.eighths()),
        })
    }

    /// What a command does, by its first word, if this knows it.
    pub(super) fn entry(&self, w0: u32) -> Option<Entry> {
        self.commands.get((w0 >> 24) as usize).copied().flatten()
    }

    /// Whether every command in a task's list runs here. Most commands need only their first byte
    /// looked at; those whose routine runs here only with some words or some commands before it, their
    /// words and the routines of the two commands before.
    #[allow(dead_code)]
    pub(super) fn runs(&self, rdram: &[u8], list: usize, size: usize) -> bool {
        self.decline_reason(rdram, list, size).is_none()
    }

    /// If a task cannot run here, returns the reason why.
    pub(super) fn decline_reason(&self, rdram: &[u8], list: usize, size: usize) -> Option<String> {
        let Some(slice) = list.checked_add(size / 8 * 8).and_then(|end| rdram.get(list..end)) else {
            return Some(format!("list outside RDRAM: list={list:#x}, size={size:#x}"));
        };
        let words = |n: usize| {
            let word = |at: usize| u32::from_be_bytes(slice[at..at + 4].try_into().unwrap());
            (word(8 * n), word(8 * n + 4))
        };
        // A command's routine and first word (none for one leading to the dispatcher).
        let routine = |n: usize| match self.entry(words(n).0) {
            Some(Entry::Runs(routine)) => Some((routine, words(n).0)),
            _ => None,
        };
        for (n, command) in slice.chunks_exact(8).enumerate() {
            let bit = 1u32.checked_shl(u32::from(command[0])).unwrap_or(0);
            if self.known & bit == 0 {
                return Some(format!("unknown opcode {:#04x} at command {n}", command[0]));
            }
            if self.checked & bit != 0 {
                let (w0, w1) = words(n);
                if let Some(Entry::Runs(r)) = self.entry(w0) {
                    if !r.accepts(w0, w1, n.checked_sub(1).and_then(routine), n.checked_sub(2).and_then(routine)) {
                        return Some(format!("routine for opcode {:#04x} rejected at command {n}: w0={w0:#010x}, w1={w1:#010x}", command[0]));
                    }
                }
            }
        }
        None
    }
}

#[cfg(test)]
impl Microcode {
    /// A microcode with the dispatcher of this fingerprint and these commands' routines, by theirs.
    pub(super) fn with(dispatcher: u64, scratch: Option<usize>, routines: &[(usize, u64)]) -> Microcode {
        let (_, dispatcher) = DISPATCHERS.iter().find(|(prints, _)| prints.contains(&dispatcher)).unwrap();
        let mut commands = [None; 32];
        for &(op, print) in routines {
            commands[op] = Some(Entry::Runs(known(print).unwrap()));
        }
        Microcode::new(dispatcher, commands, scratch).unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// IMEM from 0x1080 with these words at these addresses (the rest zero).
    fn imem(code: &[(u32, u32)]) -> Vec<u8> {
        let mut text = vec![0; 0xf80];
        for &(at, w) in code {
            text[(at - 0x1080) as usize..(at - 0x1080) as usize + 4].copy_from_slice(&w.to_be_bytes());
        }
        text
    }

    fn word_of(text: &[u8]) -> impl Fn(u32) -> Option<u32> + '_ {
        |at: u32| -> Option<u32> {
            let o = (at as usize).checked_sub(0x1080).filter(|&o| o + 4 <= text.len())?;
            Some(u32::from_be_bytes(text[o..o + 4].try_into().unwrap()))
        }
    }

    fn print_of(text: &[u8], entry: u32, dispatcher: u32) -> u64 {
        fingerprint(&word_of(text), entry, dispatcher)
    }

    /// J and JAL to an IMEM address.
    fn j(at: u32) -> u32 {
        0x0800_0000 | (at & 0xffc) >> 2
    }

    fn jal(at: u32) -> u32 {
        0x0c00_0000 | (at & 0xffc) >> 2
    }

    #[test]
    fn a_routine_is_known_by_its_code_wherever_it_is() {
        // A routine that calls a helper and goes back to the dispatcher at 0x1100, at 0x1200 and
        // its helper at 0x1300; then the same moved, and with the helper storing another way.
        let routine = |base: u32, helper: u32, store: u32| {
            imem(&[(base, 0x3341_ffff), (base + 4, jal(helper)), (base + 8, 0), (base + 12, j(0x1100)), (base + 16, 0),
                (helper, store), (helper + 4, 0x03e0_0008), (helper + 8, 0)])
        };
        let first = print_of(&routine(0x1200, 0x1300, 0x4081_0000), 0x1200, 0x1100);
        assert_eq!(print_of(&routine(0x1600, 0x1240, 0x4081_0000), 0x1600, 0x1100), first);
        assert_ne!(print_of(&routine(0x1200, 0x1300, 0x4081_1800), 0x1200, 0x1100), first);
        // The dispatcher is where the jumps go most.
        let text = imem(&[(0x1200, j(0x1100)), (0x1300, j(0x1100)), (0x1400, j(0x1180))]);
        assert_eq!(dispatcher(&word_of(&text)), 0x1100);
    }

    #[test]
    fn a_task_whose_boot_is_its_microcode_has_it_from_0x1000() {
        let mut rdram = vec![0; 0x3000];
        rdram[0x2000..0x2004].copy_from_slice(&0x1234_5678u32.to_be_bytes());
        rdram[0x2f7c..0x2f80].copy_from_slice(&0x9abc_def0u32.to_be_bytes());
        let own = Code { text: 0x2000, from: 0x1000 };
        assert_eq!((own.word(&rdram, 0x1000), own.word(&rdram, 0x1f7c), own.word(&rdram, 0x2000)),
            (Some(0x1234_5678), Some(0x9abc_def0), None));
        let loaded = Code { text: 0x2000, from: 0x1080 };
        assert_eq!((loaded.word(&rdram, 0x1000), loaded.word(&rdram, 0x1080), loaded.word(&rdram, 0x1ffc)),
            (None, Some(0x1234_5678), Some(0x9abc_def0)));
    }

    #[test]
    fn the_scratch_is_what_the_start_code_sets_s7_to() {
        // ADDI t8, ADDI s7 (0xf90); a call on the way does not end it, a branch does.
        let start = imem(&[(0x1080, 0x2018_0360), (0x1084, jal(0x1200)), (0x1088, 0), (0x108c, 0x2017_0f90)]);
        assert_eq!(start_value(&word_of(&start), 0x1080, 23), Some(0xf90));
        let branching = imem(&[(0x1080, 0x1080_0004), (0x1084, 0), (0x1088, 0x2017_0f90)]);
        assert_eq!(start_value(&word_of(&branching), 0x1080, 23), None);
    }

    #[test]
    fn code_this_does_not_know_runs_nothing() {
        // Each known dispatcher's data, its table leading into a routine that goes back to a loop of
        // jumps, which is no dispatcher this knows: the data alone makes no microcode known.
        for (_, dispatcher) in &DISPATCHERS {
            let mut rdram = vec![0; 0x4000];
            for &(at, values) in dispatcher.data {
                for (i, &v) in values.iter().enumerate() {
                    let at = 0x1000 + usize::from(at) + 2 * i;
                    rdram[at..at + 2].copy_from_slice(&v.to_be_bytes());
                }
            }
            for i in 0..usize::from(dispatcher.commands) {
                let at = 0x1000 + usize::from(dispatcher.table) + 2 * i;
                rdram[at..at + 2].copy_from_slice(&0x1200u16.to_be_bytes());
            }
            let text = imem(&[(0x1100, j(0x1100)), (0x1200, 0x2008_0001), (0x1204, j(0x1100)), (0x1300, j(0x1100))]);
            rdram[0x2000..0x2000 + text.len()].copy_from_slice(&text);
            let code = Code { text: 0x2000, from: 0x1080 };
            assert!(Microcode::of(&rdram, 0x1000, u32::from(dispatcher.size), code).is_none(), "{dispatcher:?}");
        }
    }

    #[test]
    fn a_word_kept_over_the_table_covers_two_entries() {
        // SETLOOP's word at 0xe, over a table at 0: entries 7 and 8.
        assert_eq!((0..16).filter(|&i| covers(0xe, 0, i)).collect::<Vec<_>>(), [7, 8]);
        assert_eq!((0..16).filter(|&i| covers(0x10, 0x10, i)).collect::<Vec<_>>(), [0, 1]);
    }

    #[test]
    fn tasks_run_here_only_with_what_their_commands_need_before_them() {
        // ENVSETUP1, ENVSETUP2 and ENVMIXER, the filter and SETBUFF of a microcode whose routines leave
        // their volumes and the filter's count in registers.
        let microcode = Microcode::with(0x8eec_3bec_3df7_3831, None, &[(0x12, 0x5df4_5da9_8463_92e5),
            (0x16, 0x5c86_db39_7f54_7678), (0x13, 0x8a50_6ae4_0b5f_aa83), (7, 0x8be6_d551_4159_5ba7),
            (8, 0x6124_2496_0430_75f3)]);
        let runs = |commands: &[u32]| {
            let mut rdram = vec![0; 0x100];
            for (i, &w0) in commands.iter().enumerate() {
                rdram[8 * i..8 * i + 4].copy_from_slice(&w0.to_be_bytes());
            }
            microcode.runs(&rdram, 0, 8 * commands.len())
        };
        assert!(runs(&[0x1200_0000, 0x1600_0000, 0x1300_0000, 0x0702_0160, 0x0700_03c0]));
        // ENVMIXER without both setups right before it, the filter's run without its setup, and a
        // command whose routine is not known.
        assert!(!runs(&[0x1600_0000, 0x1300_0000]));
        assert!(!runs(&[0x1200_0000, 0x1600_0000, 0x0800_0000, 0x1300_0000]));
        assert!(!runs(&[0x0700_03c0]));
        assert!(!runs(&[0x0400_0000]));
    }
}
