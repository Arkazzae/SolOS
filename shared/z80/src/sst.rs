//! Runner for the SingleStepTests/z80 suite (tests only, never part of the core).
//!
//! The suite is large third-party data, kept out of the repository. Point `SOL_Z80_SINGLESTEP` (or
//! `SOL_MD_SINGLESTEP`, the name it had when the Z80 lived in the Mega Drive crate) at a directory that holds
//! the clone `z80/` (SingleStepTests/z80), or put it in `shared/z80/tests/singlestep/`. Without it the suite
//! prints a note and passes, so `cargo test -p z80` works everywhere.
//!
//! - `SOL_Z80_ONLY=ldir,ed_b` limits a run to the files whose name contains one of the words.
//! - `SOL_Z80_EXAMPLES=n` prints n failing tests per file (default 2).
//! - `SOL_Z80_STRICT=0` only reports; by default a test whose final state differs fails the run.
//! - `SOL_Z80_FLAGS=all` also compares the undocumented flag bits 3 and 5.
//!
//! Every `SOL_Z80_*` name also answers to `SOL_MD_*`.
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

pub(crate) fn env(name: &str) -> Option<String> {
    std::env::var(format!("SOL_Z80_{name}"))
        .or_else(|_| std::env::var(format!("SOL_MD_{name}")))
        .ok()
}

pub(crate) fn suite(sub: &str) -> Option<PathBuf> {
    let root = env("SINGLESTEP")
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/singlestep"));
    let dir = root.join(sub);
    dir.is_dir().then_some(dir)
}

fn examples() -> usize {
    env("EXAMPLES").and_then(|v| v.parse().ok()).unwrap_or(2)
}

#[derive(Default)]
pub(crate) struct Tally {
    pub total: u32,
    /// Tests whose final registers or memory differ.
    pub state: u32,
    /// Tests whose cycle count differs (state may still be right).
    pub timing: u32,
    pub notes: Vec<String>,
}

impl Tally {
    pub(crate) fn add(&mut self, name: &str, state: &[String], timing: Option<String>) {
        self.total += 1;
        if !state.is_empty() {
            self.state += 1;
            if self.notes.len() < examples() {
                self.notes.push(format!("{name}: {}", state.join("; ")));
            }
        }
        if timing.is_some() {
            self.timing += 1;
        }
    }
}

/// Runs `one` over the suite's files in parallel, prints one line per file and the sums, and returns
/// the number of tests with a wrong final state. `ext` picks the files.
pub(crate) fn run(label: &str, dir: &Path, ext: &str, one: impl Fn(&Path) -> Tally + Sync) -> u32 {
    let only: Vec<String> = env("ONLY")
        .map(|v| v.split(',').map(str::to_owned).collect())
        .unwrap_or_default();
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.ends_with(ext)))
        .filter(|p| {
            only.is_empty() || {
                let n = p.file_name().unwrap().to_string_lossy();
                only.iter().any(|w| n.contains(w.as_str()))
            }
        })
        .collect();
    files.sort();
    let next = AtomicUsize::new(0);
    let done: Mutex<Vec<(String, Tally)>> = Mutex::new(Vec::new());
    let workers = std::thread::available_parallelism().map_or(4, |n| n.get()).min(16);
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(path) = files.get(i) else { break };
                    let tally = one(path);
                    let name = path.file_name().unwrap().to_string_lossy().into_owned();
                    done.lock().unwrap().push((name, tally));
                }
            });
        }
    });
    let mut done = done.into_inner().unwrap();
    done.sort_by(|a, b| a.0.cmp(&b.0));
    let (mut total, mut state, mut timing) = (0u64, 0u64, 0u64);
    println!("{label}: {} files", done.len());
    for (name, t) in &done {
        total += u64::from(t.total);
        state += u64::from(t.state);
        timing += u64::from(t.timing);
        if t.state != 0 || t.timing != 0 {
            println!("  {name:<22} {:>6} tests  state {:>5}  timing {:>5}", t.total, t.state, t.timing);
            for note in &t.notes {
                println!("      {note}");
            }
        }
    }
    println!("{label}: {total} tests, {state} with a wrong final state, {timing} with another cycle count");
    state as u32
}

/// Whether a wrong final state fails the run. It does by default; `SOL_Z80_STRICT=0` only reports, to
/// look at the numbers while something is being fixed.
pub(crate) fn strict() -> bool {
    env("STRICT").is_none_or(|v| v != "0")
}
