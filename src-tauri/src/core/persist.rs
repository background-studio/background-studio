use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::Path,
};

use serde::Serialize;
use uuid::Uuid;

pub fn write_json_transaction<T: Serialize>(file_path: &Path, value: &T) -> Result<(), String> {
    let directory = file_path
        .parent()
        .ok_or_else(|| "数据文件路径无效。".to_string())?;
    fs::create_dir_all(directory).map_err(|error| error.to_string())?;
    let temporary = file_path.with_extension(format!(
        "{}.{}.tmp",
        file_path
            .extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or("json"),
        Uuid::new_v4()
    ));
    let backup = file_path.with_extension(format!(
        "{}.bak",
        file_path
            .extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or("json")
    ));
    let bytes = format!(
        "{}\n",
        serde_json::to_string_pretty(value).map_err(|error| error.to_string())?
    );
    let mut handle = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .map_err(|error| error.to_string())?;
    handle
        .write_all(bytes.as_bytes())
        .and_then(|_| handle.sync_all())
        .map_err(|error| error.to_string())?;
    drop(handle);

    let _ = fs::remove_file(&backup);
    let had_original = file_path.exists();
    if had_original {
        fs::rename(file_path, &backup).map_err(|error| {
            let _ = fs::remove_file(&temporary);
            error.to_string()
        })?;
    }
    if let Err(error) = fs::rename(&temporary, file_path) {
        let _ = fs::remove_file(&temporary);
        if had_original {
            let _ = fs::remove_file(file_path);
            let _ = fs::rename(&backup, file_path);
        }
        return Err(error.to_string());
    }
    let _ = fs::remove_file(backup);
    Ok(())
}
