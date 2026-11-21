use serde::{de::DeserializeOwned, Serialize};
use std::fs;
use std::path::Path;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

/// Loads JSON state from disk, returning `Default` if missing or unreadable.
pub fn load_json<T: DeserializeOwned + Default>(path: &Path) -> T {
    match fs::read_to_string(path) {
        Ok(raw) => serde_json::from_str(&raw).unwrap_or_default(),
        Err(_) => T::default(),
    }
}

/// Atomically persists JSON state (`.tmp` -> `rename`) with `0600` permissions on Unix.
pub fn save_json_atomic<T: Serialize>(path: &Path, value: &T) {
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let tmp = path.with_extension("tmp");
    if let Ok(data) = serde_json::to_string_pretty(value) {
        if fs::write(&tmp, data).is_ok() {
            #[cfg(unix)]
            let _ = fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600));
            let _ = fs::rename(&tmp, path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;
    use std::env;

    #[derive(Debug, Default, PartialEq, Serialize, Deserialize)]
    struct Sample {
        offset: i64,
        flag: bool,
    }

    #[test]
    fn test_state_atomic_roundtrip() {
        let dir = env::temp_dir().join("tg_core_state_test");
        let _ = fs::create_dir_all(&dir);
        let file = dir.join("state.json");
        let s = Sample {
            offset: 4242,
            flag: true,
        };
        save_json_atomic(&file, &s);
        let loaded: Sample = load_json(&file);
        assert_eq!(loaded, s);
        let _ = fs::remove_dir_all(&dir);
    }
}
