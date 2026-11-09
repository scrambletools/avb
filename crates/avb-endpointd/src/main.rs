//! avb-endpointd: this computer's entities on one interface, each its own
//! ATDECC entity with streams that any controller can connect, with audio
//! from and to PipeWire devices apps play into or record from. It runs as
//! the person's `avb-endpointd@<interface>` unit.
//!
//! It takes gPTP from gptpd, which it starts on the interface where none
//! runs, and its raw sockets from avb-netraw, which holds `CAP_NET_RAW` in
//! its place. The entities are listed in `<interface>.toml` in the
//! configuration folder, or the file `--config` names. With no file it
//! takes triib's entities for the interface, where triib listed any, and
//! with no entities it has nothing to do and ends.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use avb_endpointd::config::{self, Config};
use avb_endpointd::status::{EXIT_BAD_CONFIG, EXIT_NO_INTERFACE, EXIT_REFUSED, EXIT_USAGE};
use avb_endpointd::{profile, runtime, status, store};

const USAGE: &str = "usage: avb-endpointd --interface NAME [--config FILE]";

fn main() -> ExitCode {
    // Raw sockets come from avb-netraw, the one file granted CAP_NET_RAW.
    #[cfg(target_os = "linux")]
    let _ = avb_net::broker::use_helper("avb-netraw", &[Path::new("/usr/lib/avb-endpointd")]);
    let mut arguments = std::env::args().skip(1);
    let mut path: Option<PathBuf> = None;
    let mut interface: Option<String> = None;
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--config" => path = arguments.next().map(PathBuf::from),
            "--interface" => interface = arguments.next(),
            "--version" | "-V" => {
                println!("avb-endpointd {}", env!("CARGO_PKG_VERSION"));
                return ExitCode::SUCCESS;
            }
            _ => {
                eprintln!("{USAGE}");
                return ExitCode::from(EXIT_USAGE);
            }
        }
    }
    let Some(interface) = interface else {
        eprintln!("{USAGE}");
        return ExitCode::from(EXIT_USAGE);
    };
    if !profile::nameable(&interface)
        || !avb_net::interfaces()
            .iter()
            .any(|found| found.name == interface)
    {
        eprintln!("no interface named {interface}");
        return ExitCode::from(EXIT_NO_INTERFACE);
    }
    let Some(path) = path.or_else(|| config::path(&interface)) else {
        eprintln!("no configuration folder for {interface}.toml; give a file with --config");
        return ExitCode::FAILURE;
    };
    let mut config = match first_config(&interface, &path) {
        Ok(Some(config)) => config,
        Ok(None) => {
            println!("no entities listed for {interface}");
            return ExitCode::SUCCESS;
        }
        Err(code) => return code,
    };
    // One of a person's daemons on the interface at a time: another would
    // claim the same entity IDs.
    let _lock = match status::take_lock(&interface) {
        Ok(Some(lock)) => lock,
        Ok(None) => {
            eprintln!("avb-endpointd runs on {interface} already");
            return ExitCode::from(EXIT_REFUSED);
        }
        Err(error) => {
            eprintln!("could not take the daemon's lock: {error}");
            return ExitCode::FAILURE;
        }
    };
    // One daemon on the interface, whoever runs it.
    let _claim = match status::claim_interface(&interface) {
        Ok(Some(claim)) => claim,
        Ok(None) => {
            eprintln!(
                "another daemon runs entities on {interface}, for this or another person or root"
            );
            return ExitCode::from(EXIT_REFUSED);
        }
        Err(error) => {
            eprintln!("could not claim {interface}: {error}");
            return ExitCode::FAILURE;
        }
    };
    let stop = Arc::new(AtomicBool::new(false));
    {
        let stop = stop.clone();
        let _ = ctrlc::set_handler(move || stop.store(true, Ordering::Relaxed));
    }
    println!(
        "avb-endpointd {} on {interface}, from {}",
        env!("CARGO_PKG_VERSION"),
        path.display()
    );
    loop {
        match runtime::Runtime::new(&interface, config, Some(path.clone()), stop.clone())
            .and_then(runtime::Runtime::run)
        {
            Ok(runtime::Exit::Stop) => return ExitCode::SUCCESS,
            Ok(runtime::Exit::Reload(changed)) => config = changed,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                eprintln!("{error}");
                return ExitCode::from(EXIT_NO_INTERFACE);
            }
            Err(error) => {
                eprintln!("{error}");
                return ExitCode::FAILURE;
            }
        }
    }
}

/// The entities to start with: the file's, or with no file triib's for
/// the interface, written to it; `None` when there are none.
fn first_config(interface: &str, path: &Path) -> Result<Option<Config>, ExitCode> {
    match config::load(path) {
        Ok(config) => Ok((!config.endpoints.is_empty()).then_some(config)),
        Err(error) if error.is_missing() => {
            let Some(config) = config::triib_entities(interface) else {
                return Ok(None);
            };
            if let Err(error) = store::save(path, &config) {
                eprintln!("{}: {error}", path.display());
                return Err(ExitCode::FAILURE);
            }
            println!(
                "took triib's {} entities on {interface} into {}",
                config.endpoints.len(),
                path.display()
            );
            Ok(Some(config))
        }
        Err(error) => {
            eprintln!("{}: {error}", path.display());
            Err(ExitCode::from(match error {
                store::LoadError::Parse(_) => EXIT_BAD_CONFIG,
                store::LoadError::Io(_) => 1,
            }))
        }
    }
}
