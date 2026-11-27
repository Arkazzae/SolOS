//! Writes the Master System and Game Gear diagnostic cartridges used in the core tests.
#[path = "../tests/support/mod.rs"]
mod support;
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let (Some(sms), Some(gg)) = (args.next(), args.next()) else {
        return Err("usage: make-sms-test-rom OUT.sms OUT.gg".into());
    };
    std::fs::write(sms, support::sms_rom())?;
    std::fs::write(gg, support::gg_rom())?;
    Ok(())
}
