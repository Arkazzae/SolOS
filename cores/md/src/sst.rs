//! Runner for the SingleStepTests/680x0 suite (tests only, never part of the core). The Z80 suite has its
//! own runner in the `z80` crate.
//!
//! The suites are large third-party data (about 2 GB), kept out of the repository. Point
//! `SOL_MD_SINGLESTEP` at a directory holding the clones `680x0/` (SingleStepTests/680x0) and `z80/`
//! (SingleStepTests/z80), or put them in `cores/md/tests/singlestep/`. Without them the suites print a
//! note and pass, so `cargo test -p md` works everywhere.
//!
//! - `SOL_MD_ONLY=ADD,MOVE.w` limits a run to the files whose name contains one of the words.
//! - `SOL_MD_EXAMPLES=n` prints n failing tests per file (default 2).
//! - `SOL_MD_STRICT=0` only reports; by default a test whose final state differs fails the run.
//!
//! Cycle counts are tallied apart from the state: the 68000 core's timing is per instruction, so a
//! timing mismatch is a measurement here, not a failure.
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

pub(crate) fn suite(sub: &str) -> Option<PathBuf> {
    let root = std::env::var_os("SOL_MD_SINGLESTEP")
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/singlestep"));
    let dir = root.join(sub);
    dir.is_dir().then_some(dir)
}

fn examples() -> usize {
    std::env::var("SOL_MD_EXAMPLES").ok().and_then(|v| v.parse().ok()).unwrap_or(2)
}

#[derive(Default)]
pub(crate) struct Tally {
    pub total: u32,
    /// Tests whose final registers or memory differ.
    pub state: u32,
    /// Tests whose cycle count differs (state may still be right).
    pub timing: u32,
    /// Tests the reference ran into an address error on, and how many of those this core got wrong.
    /// Counted apart: the frame is only seen when a game has already crashed.
    pub odd: u32,
    pub odd_state: u32,
    pub notes: Vec<String>,
}

impl Tally {
    pub(crate) fn add(&mut self, name: &str, state: &[String], timing: Option<String>, odd: bool) {
        self.total += 1;
        if odd {
            self.odd += 1;
            self.odd_state += u32::from(!state.is_empty());
            return;
        }
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
/// the number of tests with a wrong final state (address errors not counted). `ext` picks the files, `strict` is the env switch.
pub(crate) fn run(label: &str, dir: &Path, ext: &str, one: impl Fn(&Path) -> Tally + Sync) -> u32 {
    let only: Vec<String> = std::env::var("SOL_MD_ONLY")
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
    let (mut total, mut state, mut timing, mut odd, mut odd_state) = (0u64, 0u64, 0u64, 0u64, 0u64);
    println!("{label}: {} files", done.len());
    for (name, t) in &done {
        total += u64::from(t.total);
        state += u64::from(t.state);
        timing += u64::from(t.timing);
        odd += u64::from(t.odd);
        odd_state += u64::from(t.odd_state);
        if t.state != 0 || t.timing != 0 {
            println!("  {name:<22} {:>6} tests  state {:>5}  timing {:>5}  (address errors {}/{})", t.total, t.state, t.timing, t.odd_state, t.odd);
            for note in &t.notes {
                println!("      {note}");
            }
        }
    }
    println!(
        "{label}: {total} tests, {state} with a wrong final state, {timing} with another cycle count; \
         address errors apart: {odd_state} wrong of {odd}"
    );
    state as u32
}

/// Whether a wrong final state fails the run. It does by default; `SOL_MD_STRICT=0` only reports, to
/// look at the numbers while something is being fixed.
pub(crate) fn strict() -> bool {
    std::env::var("SOL_MD_STRICT").map_or(true, |v| v != "0")
}
