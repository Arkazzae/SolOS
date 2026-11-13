//! Atomic persistent SRAM and Quick Resume, shared by the desktop controls.
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};
pub struct Saves {
    pub state: PathBuf,
    ram: PathBuf,
}
impl Saves {
    pub fn new(directory: &Path, rom: &[u8], name: &str) -> std::io::Result<Self> {
        fs::create_dir_all(directory)?;
        let hash = rom.iter().fold(0xcbf29ce484222325u64, |h, &n| {
            (h ^ u64::from(n)).wrapping_mul(0x100000001b3)
        });
        let name: String = name
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
            .take(40)
            .collect();
        let base = format!("{name}-{hash:016x}");
        Ok(Self {
            state: directory.join(format!("{base}.state")),
            ram: directory.join(format!("{base}.srm")),
        })
    }
    pub fn restore(&self, md: &mut md::Md, resume: bool) -> Result<(), Box<dyn std::error::Error>> {
        if md.save_data().is_some() {
            if let Some(bytes) = read(&self.ram)? {
                md.load_save_data(&bytes)?;
            }
        }
        if resume && let Some(bytes) = read(&self.state)? {
            md.load_state(&bytes)?;
        }
        Ok(())
    }
    pub fn load(&self, md: &mut md::Md) -> Result<(), Box<dyn std::error::Error>> {
        md.load_state(&fs::read(&self.state)?)?;
        Ok(())
    }
    pub fn save(&self, md: &md::Md) -> std::io::Result<()> {
        if let Some(ram) = md.save_data() {
            atomic(&self.ram, ram)?;
        }
        atomic(&self.state, &md.save_state())
    }
    pub fn ram(&self, md: &md::Md) -> std::io::Result<()> {
        if let Some(ram) = md.save_data() {
            atomic(&self.ram, ram)?;
        }
        Ok(())
    }
}
fn read(path: &Path) -> std::io::Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}
fn atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut tmp = path.as_os_str().to_os_string();
    tmp.push(format!(".{}.tmp", std::process::id()));
    let tmp = PathBuf::from(tmp);
    let result = (|| {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&tmp, path)?;
        if let Some(parent) = path.parent() {
            fs::File::open(parent)?.sync_all()?;
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(tmp);
    }
    result
}
pub fn default_directory() -> PathBuf {
    if let Some(data) = std::env::var_os("XDG_DATA_HOME") {
        return PathBuf::from(data).join("sol-os/md");
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    if cfg!(target_os = "macos") {
        home.join("Library/Application Support/Sol OS/Mega Drive")
    } else {
        home.join(".local/share/sol-os/md")
    }
}
