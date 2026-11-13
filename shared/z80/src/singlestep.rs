//! The Z80 against SingleStepTests/z80: each test is one instruction (with its prefixes) with the whole
//! processor, the touched memory and the port traffic before and after. See `crate::sst` for the data.
//!
//! This core does not track the hidden `wz`, `p` and `q` registers, so those are not compared; they
//! only reach the undocumented flag bits 3 and 5 (BIT n,(HL), SCF/CCF, block instructions), which are
//! masked out unless `SOL_Z80_FLAGS=all` asks for them.
use super::{Bus, Z80};
use crate::sst::{self, Tally};
use serde_json::Value;
use std::io::BufReader;
use std::path::Path;

struct Machine {
    ram: Vec<u8>,
    touched: Vec<u16>,
    written: Vec<u16>,
    ports_in: Vec<(u16, u8)>,
    ports_out: Vec<(u16, u8)>,
}

impl Bus for Machine {
    fn read(&mut self, addr: u16) -> u8 {
        self.ram[usize::from(addr)]
    }
    fn write(&mut self, addr: u16, value: u8) {
        self.written.push(addr);
        self.ram[usize::from(addr)] = value;
    }
    fn input(&mut self, port: u16) -> u8 {
        self.ports_in.iter().find(|p| p.0 == port).map_or(0xff, |p| p.1)
    }
    fn output(&mut self, port: u16, value: u8) {
        self.ports_out.push((port, value));
    }
}

fn num(v: &Value, key: &str) -> u32 {
    v[key].as_u64().unwrap_or_else(|| panic!("no {key}")) as u32
}

fn run_one(m: &mut Machine, test: &Value, flag_mask: u8) -> (Vec<String>, Option<String>) {
    let (init, fin) = (&test["initial"], &test["final"]);
    let mut z = Z80::new();
    z.a = num(init, "a") as u8;
    z.f = num(init, "f") as u8;
    z.b = num(init, "b") as u8;
    z.c = num(init, "c") as u8;
    z.d = num(init, "d") as u8;
    z.e = num(init, "e") as u8;
    z.h = num(init, "h") as u8;
    z.l = num(init, "l") as u8;
    z.i = num(init, "i") as u8;
    z.r = num(init, "r") as u8;
    z.ix = num(init, "ix") as u16;
    z.iy = num(init, "iy") as u16;
    z.sp = num(init, "sp") as u16;
    z.pc = num(init, "pc") as u16;
    for (k, name) in ["af_", "bc_", "de_", "hl_"].into_iter().enumerate() {
        let pair = num(init, name);
        z.alternate[2 * k] = (pair >> 8) as u8;
        z.alternate[2 * k + 1] = pair as u8;
    }
    z.im = num(init, "im") as u8;
    z.iff1 = num(init, "iff1") != 0;
    z.iff2 = num(init, "iff2") != 0;
    z.ei_delay = u8::from(num(init, "ei") != 0);

    m.touched.clear();
    m.written.clear();
    m.ports_in.clear();
    m.ports_out.clear();
    for e in init["ram"].as_array().unwrap() {
        let addr = e[0].as_u64().unwrap() as u16;
        m.ram[usize::from(addr)] = e[1].as_u64().unwrap() as u8;
        m.touched.push(addr);
    }
    let mut ports_out_want = Vec::new();
    for p in test["ports"].as_array().map_or(&[][..], Vec::as_slice) {
        let (port, value) = (p[0].as_u64().unwrap() as u16, p[1].as_u64().unwrap() as u8);
        match p[2].as_str().unwrap() {
            "r" => m.ports_in.push((port, value)),
            _ => ports_out_want.push((port, value)),
        }
    }

    let cycles = z.step(m);

    let mut diffs = Vec::new();
    let mut cmp = |what: &str, want: u32, got: u32| {
        if want != got {
            diffs.push(format!("{what} want {want:#x} got {got:#x}"));
        }
    };
    cmp("pc", num(fin, "pc"), u32::from(z.pc));
    cmp("sp", num(fin, "sp"), u32::from(z.sp));
    cmp("a", num(fin, "a"), u32::from(z.a));
    cmp("f", num(fin, "f") & u32::from(!flag_mask), u32::from(z.f & !flag_mask));
    for (name, got) in [("b", z.b), ("c", z.c), ("d", z.d), ("e", z.e), ("h", z.h), ("l", z.l), ("i", z.i), ("r", z.r)] {
        cmp(name, num(fin, name), u32::from(got));
    }
    cmp("ix", num(fin, "ix"), u32::from(z.ix));
    cmp("iy", num(fin, "iy"), u32::from(z.iy));
    for (k, name) in ["af_", "bc_", "de_", "hl_"].into_iter().enumerate() {
        let got = u32::from(z.alternate[2 * k]) << 8 | u32::from(z.alternate[2 * k + 1]);
        // Only the alternate F carries the undocumented bits.
        let mask = if k == 0 { u32::from(!flag_mask) } else { 0xff };
        cmp(name, num(fin, name) & (0xff00 | mask), got & (0xff00 | mask));
    }
    cmp("im", num(fin, "im"), u32::from(z.im));
    cmp("iff1", num(fin, "iff1"), u32::from(z.iff1));
    cmp("iff2", num(fin, "iff2"), u32::from(z.iff2));
    cmp("ei", num(fin, "ei"), u32::from(z.ei_delay != 0));
    let want_ram = fin["ram"].as_array().unwrap();
    for e in want_ram {
        let (addr, want) = (e[0].as_u64().unwrap() as u16, e[1].as_u64().unwrap() as u32);
        let got = u32::from(m.ram[usize::from(addr)]);
        if got != want {
            diffs.push(format!("[{addr:#x}] want {want:#x} got {got:#x}"));
        }
        m.touched.push(addr);
    }
    if let Some(&addr) = m.written.iter().find(|&&a| !want_ram.iter().any(|e| e[0].as_u64() == Some(u64::from(a)))) {
        diffs.push(format!("stray write [{addr:#x}]"));
    }
    if m.ports_out != ports_out_want {
        diffs.push(format!("port writes want {ports_out_want:x?} got {:x?}", m.ports_out));
    }
    let length = test["cycles"].as_array().map_or(0, Vec::len) as u32;
    let timing = (cycles != length).then(|| format!("cycles want {length} got {cycles}"));

    // The next test starts from clean memory again.
    for &addr in m.touched.iter().chain(&m.written) {
        m.ram[usize::from(addr)] = 0;
    }
    (diffs, timing)
}

fn run_file(path: &Path, flag_mask: u8) -> Tally {
    let file = std::fs::File::open(path).unwrap();
    let tests: Vec<Value> = serde_json::from_reader(BufReader::new(file)).unwrap();
    let mut m = Machine {
        ram: vec![0; 65536],
        touched: Vec::new(),
        written: Vec::new(),
        ports_in: Vec::new(),
        ports_out: Vec::new(),
    };
    let mut tally = Tally::default();
    for test in &tests {
        let (state, timing) = run_one(&mut m, test, flag_mask);
        tally.add(test["name"].as_str().unwrap_or("?"), &state, timing);
    }
    tally
}

#[test]
fn single_step_z80() {
    let Some(dir) = sst::suite("z80/v1") else {
        println!("Z80 single-step suite not found (see sst.rs); skipped");
        return;
    };
    let flag_mask = if sst::env("FLAGS").is_some_and(|v| v == "all") { 0 } else { 0x28 };
    let wrong = sst::run("Z80", &dir, ".json", |path| run_file(path, flag_mask));
    if sst::strict() {
        assert_eq!(wrong, 0, "Z80 tests with a wrong final state");
    }
}
