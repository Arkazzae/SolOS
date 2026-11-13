//! The 68000 against SingleStepTests/680x0: each test is one instruction with the whole processor and
//! the touched memory before and after. See `crate::sst` for how to point the run at the data.
//!
//! In these tests `pc` is the address of the instruction's first word and `prefetch` holds the two
//! words the real chip has already fetched from `pc` and `pc + 2`; `ram` starts after them. This
//! core has no prefetch queue, so the two words are planted in memory before the step.
use super::{Bus, Cpu, S};
use crate::sst::{self, Tally};
use serde_json::Value;
use std::io::BufReader;
use std::path::Path;

/// 24-bit memory holding only the bytes a test names.
#[derive(Default)]
struct Mem {
    bytes: Vec<(u32, u8)>,
    written: Vec<u32>,
}

impl Mem {
    fn get(&self, addr: u32) -> Option<u8> {
        self.bytes.iter().find(|b| b.0 == addr).map(|b| b.1)
    }
    fn put(&mut self, addr: u32, value: u8) {
        match self.bytes.iter_mut().find(|b| b.0 == addr) {
            Some(b) => b.1 = value,
            None => self.bytes.push((addr, value)),
        }
    }
}

impl Bus for Mem {
    fn read8(&mut self, addr: u32) -> u8 {
        self.get(addr & 0xff_ffff).unwrap_or(0)
    }
    fn write8(&mut self, addr: u32, value: u8) {
        let addr = addr & 0xff_ffff;
        self.written.push(addr);
        self.put(addr, value);
    }
}

fn num(v: &Value, key: &str) -> u32 {
    v[key].as_u64().unwrap_or_else(|| panic!("no {key}")) as u32
}

fn run_one(test: &Value) -> (Vec<String>, Option<String>, bool) {
    let (init, fin) = (&test["initial"], &test["final"]);
    let mut cpu = Cpu::new();
    for i in 0..8 {
        cpu.d[i] = num(init, &format!("d{i}"));
    }
    for i in 0..7 {
        cpu.a[i] = num(init, &format!("a{i}"));
    }
    cpu.usp = num(init, "usp");
    cpu.ssp = num(init, "ssp");
    cpu.sr = num(init, "sr") as u16;
    cpu.a[7] = if cpu.sr & S != 0 { cpu.ssp } else { cpu.usp };
    cpu.pc = num(init, "pc");
    let mut mem = Mem::default();
    for e in init["ram"].as_array().unwrap() {
        mem.put(e[0].as_u64().unwrap() as u32, e[1].as_u64().unwrap() as u8);
    }
    for (i, word) in init["prefetch"].as_array().unwrap().iter().enumerate() {
        let word = word.as_u64().unwrap() as u16;
        let at = cpu.pc + 2 * i as u32;
        mem.put(at, (word >> 8) as u8);
        mem.put(at + 1, word as u8);
    }

    let cycles = cpu.step(&mut mem);

    let mut diffs = Vec::new();
    let mut cmp = |what: String, want: u32, got: u32| {
        if want != got {
            diffs.push(format!("{what} want {want:#x} got {got:#x}"));
        }
    };
    for i in 0..8 {
        cmp(format!("d{i}"), num(fin, &format!("d{i}")), cpu.d[i]);
    }
    for i in 0..7 {
        cmp(format!("a{i}"), num(fin, &format!("a{i}")), cpu.a[i]);
    }
    // Only the stack pointer of the other mode is kept in a field.
    let (usp, ssp) = if cpu.sr & S != 0 { (cpu.usp, cpu.a[7]) } else { (cpu.a[7], cpu.ssp) };
    cmp("usp".into(), num(fin, "usp"), usp);
    cmp("ssp".into(), num(fin, "ssp"), ssp);
    cmp("sr".into(), num(fin, "sr"), u32::from(cpu.sr));
    cmp("pc".into(), num(fin, "pc"), cpu.pc);
    let want_ram = fin["ram"].as_array().unwrap();
    for e in want_ram {
        let (addr, want) = (e[0].as_u64().unwrap() as u32, e[1].as_u64().unwrap() as u32);
        match mem.get(addr) {
            Some(got) if u32::from(got) == want => {}
            Some(got) => diffs.push(format!("[{addr:#x}] want {want:#x} got {got:#x}")),
            None => diffs.push(format!("[{addr:#x}] want {want:#x}, never set")),
        }
    }
    for &addr in &mem.written {
        if !want_ram.iter().any(|e| e[0].as_u64() == Some(u64::from(addr))) {
            diffs.push(format!("stray write [{addr:#x}]"));
            break;
        }
    }
    let length = num(test, "length");
    let timing = (cycles != length).then(|| format!("length want {length} got {cycles}"));
    // The reference only reads vector 3 (bytes 12-15) when the instruction ended in an address error.
    let odd = init["ram"].as_array().unwrap().iter().any(|e| (12..16).contains(&e[0].as_u64().unwrap()));
    (diffs, timing, odd)
}

/// Tests left out of the count. Two expect an ASL.b to change bits 8-31 of the register, which no byte shift
/// can (the other 8063 ASL.b tests, and every other byte shift, agree with this core). The third is the only
/// divide-by-zero in the whole suite: its frame holds the address of the DIVU itself as the stacked PC where the
/// manual says the next instruction; one sample is not enough to model it, so it is not.
const ANOMALIES: [&str; 3] = [
    "e502 [ASL.b Q, D2] 1583",
    "e502 [ASL.b Q, D2] 1761",
    "80ef [DIVU (d16, A7), D0] 5745",
];

fn run_file(path: &Path) -> Tally {
    let file = std::fs::File::open(path).unwrap();
    let tests: Vec<Value> =
        serde_json::from_reader(BufReader::new(flate2::read::GzDecoder::new(file))).unwrap();
    let mut tally = Tally::default();
    for test in &tests {
        let name = test["name"].as_str().unwrap_or("?");
        if ANOMALIES.contains(&name) {
            continue;
        }
        let (state, timing, odd) = run_one(test);
        tally.add(name, &state, timing, odd);
    }
    tally
}

#[test]
fn single_step_68000() {
    let Some(dir) = sst::suite("680x0/68000/v1") else {
        println!("68000 single-step suite not found (see sst.rs); skipped");
        return;
    };
    let wrong = sst::run("68000", &dir, ".json.gz", run_file);
    if sst::strict() {
        assert_eq!(wrong, 0, "68000 tests with a wrong final state");
    }
}
