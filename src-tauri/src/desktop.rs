use std::{path::Path, process::Command};

use winreg::{enums::HKEY_CURRENT_USER, RegKey};

const AUTOSTART_NAME: &str = "Background Studio";

pub fn sync_autostart(enabled: bool, start_hidden: bool) -> Result<(), String> {
    let current_exe = std::env::current_exe().map_err(|error| error.to_string())?;
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let (run, _) = hkcu
        .create_subkey(r"Software\Microsoft\Windows\CurrentVersion\Run")
        .map_err(|error| error.to_string())?;
    if enabled {
        let mut command = format!("\"{}\"", current_exe.display());
        if start_hidden {
            command.push_str(" --hidden");
        }
        run.set_value(AUTOSTART_NAME, &command)
            .map_err(|error| error.to_string())
    } else {
        match run.delete_value(AUTOSTART_NAME) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.to_string()),
        }
    }
}

pub fn open_data_directory(path: &Path) -> Result<(), String> {
    Command::new("explorer")
        .arg(path)
        .spawn()
        .map(|_| ())
        .map_err(|error| error.to_string())
}
