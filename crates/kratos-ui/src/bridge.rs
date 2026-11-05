//! Registering the browser bridge, from the settings window.
//!
//! An extension loaded unpacked has no id until the browser gives it one, and
//! the native-messaging manifest has to name both that id and this machine's
//! absolute path to `kratos-host.exe`. That is the one piece of setup nobody
//! can guess for you, so the UI asks for the id and writes the rest: the
//! manifest, and one registry key per Chromium brand pointing at it.
//!
//! The same work the installer script does, done here so that pasting the id
//! into a box is the whole procedure.

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};

/// Must match `name` in the manifest and `HOST` in extension/background.js.
pub const HOST_NAME: &str = "ai.kratos.bridge";

/// The brands that read a per-hive NativeMessagingHosts key.
///
/// Vivaldi and Opera are Chromium too, but they read Chrome's hive, so
/// registering `chrome` already covers them.
pub const BRANDS: [(&str, &str); 3] = [
    ("brave", r"Software\BraveSoftware\Brave-Browser\NativeMessagingHosts"),
    ("chrome", r"Software\Google\Chrome\NativeMessagingHosts"),
    ("edge", r"Software\Microsoft\Edge\NativeMessagingHosts"),
];

/// What a registration attempt did, per brand, for display.
#[derive(Debug)]
pub struct Report {
    pub manifest: PathBuf,
    pub host_exe: PathBuf,
    pub lines: Vec<(String, Result<String, String>)>,
}

/// Chromium ids are 32 letters in a..p -- a base16 encoding over that
/// alphabet. Checking it here turns a mistyped id into a message instead of a
/// manifest the browser silently ignores.
pub fn valid_id(id: &str) -> bool {
    id.len() == 32 && id.bytes().all(|b| b.is_ascii_lowercase() && b <= b'p')
}

/// The manifest text for an id and a host path.
///
/// Hand-rolled rather than serialised so the field order is stable and the
/// file stays diffable; the only values that vary are two paths we control.
pub fn manifest_json(id: &str, host_exe: &Path) -> String {
    let path = host_exe.display().to_string().replace('\\', "\\\\");
    format!(
        "{{\n  \"name\": \"{HOST_NAME}\",\n  \"description\": \"kratos browser bridge\",\n  \
         \"path\": \"{path}\",\n  \"type\": \"stdio\",\n  \"allowed_origins\": [\n    \
         \"chrome-extension://{id}/\"\n  ]\n}}\n"
    )
}

/// Where the manifest lives: per-user, no elevation, no PATH assumptions.
pub fn manifest_path() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join("kratos").join("native-host.json")
}

/// `kratos-host.exe` beside whatever is running, which is where cargo puts it.
pub fn host_exe() -> Option<PathBuf> {
    let dir = std::env::current_exe().ok()?.parent()?.to_path_buf();
    let exe = dir.join("kratos-host.exe");
    exe.exists().then_some(exe)
}

/// The id currently registered, read back out of the manifest.
///
/// Reported so the box is not blank when a registration already exists, and
/// so a stale one is visible rather than merely not working.
pub fn registered_id() -> Option<String> {
    let text = std::fs::read_to_string(manifest_path()).ok()?;
    let (_, rest) = text.split_once("chrome-extension://")?;
    let (id, _) = rest.split_once('/')?;
    valid_id(id).then(|| id.to_string())
}

/// Write the manifest and point every requested brand at it.
pub fn register(id: &str, brands: &[String]) -> Result<Report, String> {
    let id = id.trim();
    if !valid_id(id) {
        return Err("That is not an extension id. Expected 32 letters a-p, as shown on the \
                    browser's extensions page."
            .to_string());
    }
    let host = host_exe().ok_or(
        "kratos-host.exe is not next to this program. Build the workspace first: \
         cargo build --release",
    )?;

    let manifest = manifest_path();
    if let Some(dir) = manifest.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    // Plain UTF-8, no BOM: Chromium's parser rejects one.
    std::fs::write(&manifest, manifest_json(id, &host))
        .map_err(|e| format!("{}: {e}", manifest.display()))?;

    let mut lines = Vec::new();
    for brand in brands {
        let Some((_, subkey)) = BRANDS.iter().find(|(b, _)| b == brand) else {
            continue;
        };
        let key = format!(r"{subkey}\{HOST_NAME}");
        let result = set_default_value(&key, &manifest.display().to_string())
            .map(|()| format!(r"HKCU\{key}"));
        lines.push((brand.clone(), result));
    }

    Ok(Report { manifest, host_exe: host, lines })
}

/// Remove the per-brand keys, leaving the manifest in place.
pub fn unregister(brands: &[String]) -> Vec<(String, Result<(), String>)> {
    brands
        .iter()
        .filter_map(|brand| {
            let (_, subkey) = BRANDS.iter().find(|(b, _)| b == brand)?;
            let key = format!(r"{subkey}\{HOST_NAME}");
            Some((brand.clone(), delete_key(&key)))
        })
        .collect()
}

fn wide(s: &str) -> Vec<u16> {
    OsStr::new(s).encode_wide().chain(std::iter::once(0)).collect()
}

/// `HKCU\<subkey>` default value := `value`, creating the key if needed.
fn set_default_value(subkey: &str, value: &str) -> Result<(), String> {
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegCreateKeyExW, RegSetValueExW, HKEY, HKEY_CURRENT_USER, KEY_SET_VALUE,
        REG_OPTION_NON_VOLATILE, REG_SZ,
    };

    let path = wide(subkey);
    let data = wide(value);
    let mut key: HKEY = std::ptr::null_mut();

    // SAFETY: path is NUL-terminated and lives past the call; key is a valid
    // out-pointer; the optional class, security and disposition arguments are
    // all documented as accepting null.
    let rc = unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            path.as_ptr(),
            0,
            std::ptr::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE,
            std::ptr::null(),
            &mut key,
            std::ptr::null_mut(),
        )
    };
    if rc != 0 {
        return Err(format!("cannot create the key (error {rc})"));
    }

    // SAFETY: key came back from RegCreateKeyExW above. A null value name
    // means the default value. The length is in bytes and includes the
    // terminator, which REG_SZ requires.
    let rc = unsafe {
        let bytes = (data.len() * 2) as u32;
        let rc = RegSetValueExW(key, std::ptr::null(), 0, REG_SZ, data.as_ptr().cast(), bytes);
        RegCloseKey(key);
        rc
    };
    if rc != 0 {
        return Err(format!("cannot write the value (error {rc})"));
    }
    Ok(())
}

fn delete_key(subkey: &str) -> Result<(), String> {
    use windows_sys::Win32::System::Registry::{RegDeleteKeyW, HKEY_CURRENT_USER};

    let path = wide(subkey);
    // SAFETY: path is NUL-terminated and outlives the call.
    let rc = unsafe { RegDeleteKeyW(HKEY_CURRENT_USER, path.as_ptr()) };
    // 2 is ERROR_FILE_NOT_FOUND: already absent is the state we wanted.
    if rc == 0 || rc == 2 {
        Ok(())
    } else {
        Err(format!("error {rc}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_outside_the_chromium_alphabet_are_rejected() {
        assert!(valid_id("ldgfddpecnjhdahallkmdlpiglnflige"));
        // q..z never appear in a Chromium id, so this is a paste of something
        // else -- catching it here beats a manifest the browser ignores.
        assert!(!valid_id("zdgfddpecnjhdahallkmdlpiglnflige"));
        assert!(!valid_id("LDGFDDPECNJHDAHALLKMDLPIGLNFLIGE"));
        assert!(!valid_id("ldgfddpecnjhdahallkmdlpiglnflig"));
        assert!(!valid_id(""));
        assert!(!valid_id("ldgfddpecnjhdahallkmdlpiglnflige "));
    }

    #[test]
    fn the_manifest_escapes_windows_separators() {
        let json = manifest_json(
            "ldgfddpecnjhdahallkmdlpiglnflige",
            Path::new(r"D:\Kratos\target\release\kratos-host.exe"),
        );
        // A raw backslash here would make the file invalid JSON and the host
        // would simply never launch.
        assert!(json.contains(r"D:\\Kratos\\target\\release\\kratos-host.exe"));
        assert!(json.contains("chrome-extension://ldgfddpecnjhdahallkmdlpiglnflige/"));
        assert!(json.contains(HOST_NAME));
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
        assert_eq!(parsed["type"], "stdio");
    }

    #[test]
    fn an_id_round_trips_through_the_manifest() {
        let json = manifest_json("abcdefghijklmnopabcdefghijklmnop", Path::new("x.exe"));
        let (_, rest) = json.split_once("chrome-extension://").unwrap();
        let (id, _) = rest.split_once('/').unwrap();
        assert_eq!(id, "abcdefghijklmnopabcdefghijklmnop");
    }

    #[test]
    fn registering_refuses_a_bad_id_before_touching_the_disk() {
        let err = register("nope", &["brave".to_string()]).unwrap_err();
        assert!(err.contains("32 letters"), "{err}");
    }
}
