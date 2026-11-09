//! How loud each running stream is, for the apps' meters:
//! `<interface>.meters` in the runtime folder, written 20 times a second
//! while an app asks for it, as it does by touching
//! `<interface>.meters-wanted` there while it shows them.

use std::fs::File;
use std::io::{self, Write};
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

/// How often the daemon writes the meters, and the app reads them.
pub const EVERY: Duration = Duration::from_millis(50);

/// How long after an app last asked the daemon goes on writing.
pub const WANTED_FOR: Duration = Duration::from_secs(3);

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Meters {
    /// Counts the writes, so a reader tells a new one from the last.
    pub sequence: u64,
    #[serde(rename = "stream", default, skip_serializing_if = "Vec::is_empty")]
    pub streams: Vec<StreamMeter>,
}

/// One of an entity's streams while it runs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StreamMeter {
    /// As text, `0x` and sixteen hex digits.
    pub entity_id: String,
    pub input: bool,
    pub index: u16,
    /// The loudest sample in each of its first two channels over the last
    /// two writes, from 0 to 1, so a reader taking one write in two still
    /// sees every peak.
    pub peaks: [f32; 2],
}

fn meters_path(interface: &str) -> Option<PathBuf> {
    crate::store::runtime_dir().map(|dir| dir.join(format!("{interface}.meters")))
}

fn wanted_path(interface: &str) -> Option<PathBuf> {
    crate::store::runtime_dir().map(|dir| dir.join(format!("{interface}.meters-wanted")))
}

/// Asks the daemon on `interface` for its meters for the next
/// [`WANTED_FOR`].
pub fn want(interface: &str) {
    let Some(path) = wanted_path(interface) else {
        return;
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(file) = File::options().create(true).append(true).open(&path) {
        let _ = file.set_modified(SystemTime::now());
    }
}

/// Whether an app asked for the meters on `interface` within
/// [`WANTED_FOR`].
pub fn wanted(interface: &str) -> bool {
    wanted_path(interface)
        .and_then(|path| std::fs::metadata(path).ok())
        .and_then(|metadata| metadata.modified().ok())
        .and_then(|asked| asked.elapsed().ok())
        .is_some_and(|since| since < WANTED_FOR)
}

/// The meters the daemon on `interface` last wrote.
pub fn read(interface: &str) -> Option<Meters> {
    crate::store::load(&meters_path(interface)?).ok()
}

/// Replaces the meters, by a sibling file renamed over them so a reader
/// never sees half, without syncing it to disk: they last a moment.
pub fn write(interface: &str, meters: &Meters) -> io::Result<()> {
    let path = meters_path(interface).ok_or_else(|| io::Error::other("no runtime folder"))?;
    let text = toml::to_string(meters).map_err(io::Error::other)?;
    let temp = path.with_extension("meters.new");
    File::create(&temp)?.write_all(text.as_bytes())?;
    std::fs::rename(&temp, &path)
}

/// Removes the meters on `interface`, as its daemon stops.
pub fn remove(interface: &str) {
    if let Some(path) = meters_path(interface) {
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meters_read_back() {
        let meters = Meters {
            sequence: 7,
            streams: vec![StreamMeter {
                entity_id: "0xa010a3ff00a51726".into(),
                input: true,
                index: 1,
                peaks: [0.5, 0.25],
            }],
        };
        let text = toml::to_string(&meters).unwrap();
        assert_eq!(toml::from_str::<Meters>(&text).unwrap(), meters);
        // No streams running.
        let none = Meters {
            sequence: 8,
            streams: Vec::new(),
        };
        let text = toml::to_string(&none).unwrap();
        assert_eq!(toml::from_str::<Meters>(&text).unwrap(), none);
    }
}
