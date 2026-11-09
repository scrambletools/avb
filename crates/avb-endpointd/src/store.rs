//! Where the daemon and the apps keep its files, and writing them so a
//! crash never leaves half of one.

use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use serde::Serialize;
use serde::de::DeserializeOwned;

/// The folder name under each base folder.
const FOLDER: &str = "avb-endpointd";

/// Why a file could not be used.
#[derive(Debug)]
pub enum LoadError {
    Io(io::Error),
    Parse(toml::de::Error),
}

impl LoadError {
    /// Whether the file is not there.
    pub fn is_missing(&self) -> bool {
        matches!(self, LoadError::Io(error) if error.kind() == io::ErrorKind::NotFound)
    }
}

impl fmt::Display for LoadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "cannot read it: {error}"),
            Self::Parse(error) => write!(formatter, "it is not valid: {error}"),
        }
    }
}

impl std::error::Error for LoadError {}

/// Loads the TOML file at `path`, as it is.
pub fn load<T: DeserializeOwned>(path: &Path) -> Result<T, LoadError> {
    let text = fs::read_to_string(path).map_err(LoadError::Io)?;
    toml::from_str(&text).map_err(LoadError::Parse)
}

/// Writes `value` as TOML in place of `path`, creating the folder first.
pub fn save<T: Serialize>(path: &Path, value: &T) -> io::Result<()> {
    let text = toml::to_string_pretty(value).map_err(io::Error::other)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    write(path, text.as_bytes())
}

/// The configuration folder: `$XDG_CONFIG_HOME/avb-endpointd`, else
/// `~/.config/avb-endpointd`.
pub fn config_dir() -> Option<PathBuf> {
    base("XDG_CONFIG_HOME", ".config").map(|dir| dir.join(FOLDER))
}

/// The folder for the status, the meters and the locks:
/// `$XDG_RUNTIME_DIR/avb-endpointd`. macOS has no runtime folder; there it
/// is under `$TMPDIR`, which macOS gives each user.
pub fn runtime_dir() -> Option<PathBuf> {
    std::env::var_os("XDG_RUNTIME_DIR")
        .or_else(|| {
            cfg!(target_os = "macos")
                .then(|| std::env::var_os("TMPDIR"))
                .flatten()
        })
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .map(|dir| dir.join(FOLDER))
}

/// triib's data folder, where triib kept the entities before the daemon
/// was its own: `$XDG_DATA_HOME/triib`, else `~/.local/share/triib`.
pub fn triib_data_dir() -> Option<PathBuf> {
    base("XDG_DATA_HOME", ".local/share").map(|dir| dir.join("triib"))
}

/// An XDG base folder, or its default under the home folder.
fn base(variable: &str, under_home: &str) -> Option<PathBuf> {
    std::env::var_os(variable)
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .filter(|path| path.is_absolute())
                .map(|home| home.join(under_home))
        })
}

static TEMP_COUNTER: AtomicU32 = AtomicU32::new(0);

/// Replaces `path` with `contents` so readers see either the old file or
/// the new one, never part of one. An existing file keeps its
/// permissions.
pub fn write(path: &Path, contents: &[u8]) -> io::Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let permissions = fs::metadata(path)
        .ok()
        .map(|metadata| metadata.permissions());
    let temp = temp_sibling(path);
    let result = (|| {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o644);
        let mut file = options.open(&temp)?;
        file.write_all(contents)?;
        if let Some(permissions) = permissions {
            file.set_permissions(permissions)?;
        }
        file.sync_all()?;
        fs::rename(&temp, path)?;
        sync_folder(parent)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

/// Makes the rename itself durable.
#[cfg(unix)]
fn sync_folder(folder: &Path) -> io::Result<()> {
    fs::File::open(folder)?.sync_all()
}

#[cfg(not(unix))]
fn sync_folder(_folder: &Path) -> io::Result<()> {
    Ok(())
}

fn temp_sibling(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy())
        .unwrap_or_default();
    let unique = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    path.with_file_name(format!(
        ".{name}.{FOLDER}-{}-{unique}.tmp",
        std::process::id()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("avb-endpointd-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_file_is_replaced_whole_with_nothing_left_beside_it() {
        let dir = scratch("store");
        let path = dir.join("file.toml");
        write(&path, b"first").unwrap();
        write(&path, b"second").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"second");
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_file_says_so() {
        let dir = scratch("missing");
        let error = load::<toml::Table>(&dir.join("none.toml")).unwrap_err();
        assert!(error.is_missing());
        fs::write(dir.join("bad.toml"), "count = ").unwrap();
        let error = load::<toml::Table>(&dir.join("bad.toml")).unwrap_err();
        assert!(!error.is_missing());
        let _ = fs::remove_dir_all(&dir);
    }
}
