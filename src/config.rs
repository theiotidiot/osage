//! Profile persistence (`~/.config/osage/profiles.toml`) and OS keychain access.
//!
//! Two rules govern this module:
//!
//! * Secrets never touch the TOML file. Profiles carry only a `secret_ref`
//!   pointing at an OS keychain entry.
//! * Writes are atomic. `save_profiles` renames a fully written temporary file
//!   over the target, so an interrupted write can never truncate the user's
//!   profiles.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::types::Profile;

/// Keychain service used when a `secret_ref` carries no explicit one.
const DEFAULT_KEYCHAIN_SERVICE: &str = "osage";

/// On-disk shape of `profiles.toml` — an array of tables named `profile`:
///
/// ```toml
/// [[profile]]
/// id = "prod-pg"
/// ...
/// ```
#[derive(Debug, Default, Serialize, Deserialize)]
struct ProfilesFile {
    #[serde(default)]
    profile: Vec<Profile>,
}

/// Directory holding osage's configuration.
///
/// Resolution order:
/// 1. `$OSAGE_CONFIG_DIR` — a full override, used verbatim.
/// 2. `$XDG_CONFIG_HOME/osage`.
/// 3. `~/.config/osage`.
fn config_dir() -> PathBuf {
    if let Some(dir) = env_path("OSAGE_CONFIG_DIR") {
        return dir;
    }
    if let Some(dir) = env_path("XDG_CONFIG_HOME") {
        return dir.join("osage");
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".config")
        .join("osage")
}

/// A non-empty environment variable read as a path.
fn env_path(key: &str) -> Option<PathBuf> {
    match std::env::var_os(key) {
        Some(value) if !value.is_empty() => Some(PathBuf::from(value)),
        _ => None,
    }
}

/// Path of the profiles file, honouring `$XDG_CONFIG_HOME`/`OSAGE_CONFIG_DIR`.
pub fn profiles_path() -> PathBuf {
    config_dir().join("profiles.toml")
}

/// Load profiles. A missing file is not an error — it yields an empty list.
pub fn load_profiles() -> Result<Vec<Profile>, String> {
    let path = profiles_path();
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("failed to read {}: {e}", path.display())),
    };
    let parsed: ProfilesFile =
        toml::from_str(&text).map_err(|e| format!("failed to parse {}: {e}", path.display()))?;
    Ok(parsed.profile)
}

/// Atomically write profiles, creating parent directories as needed.
///
/// The file is written to a `.tmp` sibling and then renamed over the target, so
/// readers see either the old file or the new one — never a partial write. On
/// Unix the result is mode 0o600: it holds hostnames and usernames.
pub fn save_profiles(profiles: &[Profile]) -> Result<(), String> {
    let path = profiles_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("failed to create {}: {e}", parent.display()))?;
    }

    let doc = ProfilesFile {
        profile: profiles.to_vec(),
    };
    let text =
        toml::to_string_pretty(&doc).map_err(|e| format!("failed to serialize profiles: {e}"))?;

    let tmp = tmp_sibling(&path);
    write_private(&tmp, &text)?;
    if let Err(e) = std::fs::rename(&tmp, &path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("failed to replace {}: {e}", path.display()));
    }
    Ok(())
}

/// `profiles.toml` -> `profiles.toml.tmp`, in the same directory so the rename
/// stays within one filesystem (and therefore stays atomic).
fn tmp_sibling(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".tmp");
    path.with_file_name(name)
}

/// Write `text` to `path`, owner-readable only on Unix.
fn write_private(path: &Path, text: &str) -> Result<(), String> {
    use std::io::Write;

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }

    let mut file = options
        .open(path)
        .map_err(|e| format!("failed to open {}: {e}", path.display()))?;

    // An existing temp file keeps its old mode, so set it explicitly too.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(|e| format!("failed to chmod {}: {e}", path.display()))?;
    }

    file.write_all(text.as_bytes())
        .map_err(|e| format!("failed to write {}: {e}", path.display()))?;
    file.sync_all()
        .map_err(|e| format!("failed to flush {}: {e}", path.display()))?;
    Ok(())
}

// ---- keychain -----------------------------------------------------------

/// Split a `secret_ref` into a keyring (service, username) pair.
///
/// The convention is `"osage/<profile-id>"`, which maps to service `"osage"`
/// and username `"<profile-id>"`. A reference without a `/` is treated as a
/// bare profile id under the default `"osage"` service.
fn split_secret_ref(secret_ref: &str) -> (&str, &str) {
    match secret_ref.split_once('/') {
        Some((service, user)) if !service.is_empty() && !user.is_empty() => (service, user),
        _ => (DEFAULT_KEYCHAIN_SERVICE, secret_ref),
    }
}

fn entry(secret_ref: &str) -> Result<keyring::Entry, String> {
    let (service, user) = split_secret_ref(secret_ref);
    keyring::Entry::new(service, user)
        .map_err(|e| format!("keychain unavailable for {secret_ref}: {e}"))
}

/// Read a secret out of the OS keychain. `None` when no entry exists.
pub fn get_secret(secret_ref: &str) -> Result<Option<String>, String> {
    match entry(secret_ref)?.get_password() {
        Ok(secret) => Ok(Some(secret)),
        Err(keyring::Error::NoEntry) => Ok(None),
        // Never include the secret (or the underlying blob) in the message.
        Err(e) => Err(format!("keychain read failed for {secret_ref}: {e}")),
    }
}

/// Store a secret in the OS keychain.
pub fn set_secret(secret_ref: &str, secret: &str) -> Result<(), String> {
    entry(secret_ref)?
        .set_password(secret)
        .map_err(|e| format!("keychain write failed for {secret_ref}: {e}"))
}

/// Remove a secret from the OS keychain. Missing entries are not an error.
pub fn delete_secret(secret_ref: &str) -> Result<(), String> {
    match entry(secret_ref)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(format!("keychain delete failed for {secret_ref}: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Color;
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Every test in here mutates process-wide environment variables, so they
    /// must not run concurrently.
    static MUTEX: Mutex<()> = Mutex::new(());
    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    /// Points `OSAGE_CONFIG_DIR` at a fresh temp directory (and clears
    /// `XDG_CONFIG_HOME`), restoring the environment on drop.
    struct TempConfig {
        dir: PathBuf,
        _guard: std::sync::MutexGuard<'static, ()>,
        prev_osage: Option<std::ffi::OsString>,
        prev_xdg: Option<std::ffi::OsString>,
    }

    impl TempConfig {
        fn new(label: &str) -> Self {
            let guard = MUTEX.lock().unwrap_or_else(|e| e.into_inner());
            let n = COUNTER.fetch_add(1, Ordering::SeqCst);
            let dir = std::env::temp_dir().join(format!(
                "osage-config-test-{}-{label}-{n}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            let prev_osage = std::env::var_os("OSAGE_CONFIG_DIR");
            let prev_xdg = std::env::var_os("XDG_CONFIG_HOME");
            unsafe {
                std::env::set_var("OSAGE_CONFIG_DIR", &dir);
                std::env::remove_var("XDG_CONFIG_HOME");
            }
            Self {
                dir,
                _guard: guard,
                prev_osage,
                prev_xdg,
            }
        }
    }

    impl Drop for TempConfig {
        fn drop(&mut self) {
            unsafe {
                match &self.prev_osage {
                    Some(v) => std::env::set_var("OSAGE_CONFIG_DIR", v),
                    None => std::env::remove_var("OSAGE_CONFIG_DIR"),
                }
                match &self.prev_xdg {
                    Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
                    None => std::env::remove_var("XDG_CONFIG_HOME"),
                }
            }
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn sample_profiles() -> Vec<Profile> {
        let mut options = HashMap::new();
        options.insert("adbc.connection.autocommit".to_string(), "true".to_string());
        options.insert("sslmode".to_string(), "require".to_string());
        vec![
            Profile {
                id: "prod-pg".to_string(),
                name: "Production Postgres".to_string(),
                driver: "postgresql".to_string(),
                uri: "postgresql://host:5432/db".to_string(),
                username: Some("readonly".to_string()),
                secret_ref: Some("osage/prod-pg".to_string()),
                options,
                color: Some(Color::Red),
            },
            Profile {
                id: "local-duck".to_string(),
                name: "Local DuckDB".to_string(),
                driver: "duckdb".to_string(),
                uri: ":memory:".to_string(),
                username: None,
                secret_ref: None,
                options: HashMap::new(),
                color: None,
            },
        ]
    }

    #[test]
    fn profiles_path_honours_osage_config_dir() {
        let tmp = TempConfig::new("path");
        assert_eq!(profiles_path(), tmp.dir.join("profiles.toml"));
    }

    #[test]
    fn profiles_path_falls_back_to_xdg_config_home() {
        let tmp = TempConfig::new("xdg");
        unsafe {
            std::env::remove_var("OSAGE_CONFIG_DIR");
            std::env::set_var("XDG_CONFIG_HOME", &tmp.dir);
        }
        assert_eq!(profiles_path(), tmp.dir.join("osage").join("profiles.toml"));
    }

    #[test]
    fn missing_file_loads_as_empty() {
        let _tmp = TempConfig::new("missing");
        assert_eq!(load_profiles().unwrap(), Vec::new());
    }

    #[test]
    fn round_trips_profiles() {
        let _tmp = TempConfig::new("roundtrip");
        let profiles = sample_profiles();
        save_profiles(&profiles).unwrap();
        assert_eq!(load_profiles().unwrap(), profiles);
    }

    #[test]
    fn saved_file_uses_array_of_tables_and_hides_secrets() {
        let _tmp = TempConfig::new("shape");
        save_profiles(&sample_profiles()).unwrap();
        let text = std::fs::read_to_string(profiles_path()).unwrap();

        assert!(
            text.contains("[[profile]]"),
            "expected array-of-tables:\n{text}"
        );
        assert!(
            text.contains("[profile.options]"),
            "expected options table:\n{text}"
        );
        assert!(text.contains("secret_ref = \"osage/prod-pg\""));
        assert!(
            !text.contains("password"),
            "profiles.toml must never hold a password:\n{text}"
        );
        assert!(!text.contains("secret ="));

        // Fields skipped when `None` must not be emitted at all.
        assert_eq!(text.matches("username =").count(), 1);
        assert_eq!(text.matches("secret_ref =").count(), 1);
        assert_eq!(text.matches("color =").count(), 1);
    }

    #[test]
    fn lowercase_color_names_parse_and_round_trip() {
        let _tmp = TempConfig::new("color");
        std::fs::create_dir_all(&_tmp.dir).unwrap();
        std::fs::write(
            profiles_path(),
            "[[profile]]\nid = \"a\"\nname = \"A\"\ndriver = \"duckdb\"\n\
             uri = \":memory:\"\ncolor = \"red\"\n",
        )
        .unwrap();

        let loaded = load_profiles().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].color, Some(Color::Red));

        // …and survives a save/load cycle in its serialized (`"Red"`) form.
        save_profiles(&loaded).unwrap();
        assert_eq!(load_profiles().unwrap(), loaded);
    }

    #[test]
    fn parse_errors_name_the_file() {
        let _tmp = TempConfig::new("bad");
        std::fs::create_dir_all(&_tmp.dir).unwrap();
        std::fs::write(profiles_path(), "this is not = = toml").unwrap();
        let err = load_profiles().unwrap_err();
        assert!(err.contains("profiles.toml"), "{err}");
    }

    #[test]
    fn save_overwrites_atomically_and_leaves_no_temp_file() {
        let _tmp = TempConfig::new("atomic");
        save_profiles(&sample_profiles()).unwrap();
        save_profiles(&sample_profiles()[..1]).unwrap();

        assert_eq!(load_profiles().unwrap().len(), 1);
        assert!(!tmp_sibling(&profiles_path()).exists());
    }

    #[cfg(unix)]
    #[test]
    fn saved_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let _tmp = TempConfig::new("mode");
        save_profiles(&sample_profiles()).unwrap();
        let mode = std::fs::metadata(profiles_path())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "got {:o}", mode & 0o777);
    }

    #[test]
    fn secret_refs_split_into_service_and_user() {
        assert_eq!(split_secret_ref("osage/prod-pg"), ("osage", "prod-pg"));
        assert_eq!(split_secret_ref("prod-pg"), ("osage", "prod-pg"));
        assert_eq!(split_secret_ref("/prod-pg"), ("osage", "/prod-pg"));
    }

    /// Touches the real login keychain (and would prompt), so it is opt-in.
    #[test]
    #[ignore = "touches the real OS keychain"]
    fn keychain_round_trip() {
        let reference = "osage/test-ignored-entry";
        assert_eq!(get_secret(reference).unwrap(), None);
        set_secret(reference, "hunter2").unwrap();
        assert_eq!(get_secret(reference).unwrap().as_deref(), Some("hunter2"));
        delete_secret(reference).unwrap();
        delete_secret(reference).unwrap();
        assert_eq!(get_secret(reference).unwrap(), None);
    }
}
