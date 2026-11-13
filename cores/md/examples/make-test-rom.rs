//! Writes our original diagnostic ROM for run-md or solhost.
#[path = "../tests/support/mod.rs"]
mod support;
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args()
        .nth(1)
        .ok_or("usage: make-test-rom OUT.md")?;
    std::fs::write(path, support::rom())?;
    Ok(())
}
