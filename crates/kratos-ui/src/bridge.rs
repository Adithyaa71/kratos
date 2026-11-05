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

/// The hives each brand might read, under HKCU.
///
/// More than one per brand on purpose. A release Brave reads its own name, but
/// the beta and nightly channels read theirs, a plain Chromium build reads
/// `Software\Chromium`, and Vivaldi and Opera read Chrome's. Writing only the
/// obvious one produces the worst kind of failure: a registration that works
/// in the browser you tested and silently does not exist in the next one, with
/// nothing to see but "Specified native messaging host not found".
///
/// Writing a key that no browser ever reads costs nothing.
pub const BRANDS: [(&str, &[&str]); 3] = [
    (
        "brave",
        &[
            r"Software\BraveSoftware\Brave-Browser\NativeMessagingHosts",
            r"Software\BraveSoftware\Brave-Browser-Beta\NativeMessagingHosts",
            r"Software\BraveSoftware\Brave-Browser-Nightly\NativeMessagingHosts",
            r"Software\BraveSoftware\Brave-Browser-Dev\NativeMessagingHosts",
        ],
    ),
    (
        "chrome",
        &[
            r"Software\Google\Chrome\NativeMessagingHosts",
            r"Software\Chromium\NativeMessagingHosts",
        ],
    ),
    ("edge", &[r"Software\Microsoft\Edge\NativeMessagingHosts"]),
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
    let target = manifest.display().to_string();
    for brand in brands {
        let Some((_, subkeys)) = BRANDS.iter().find(|(b, _)| b == brand) else {
            continue;
        };
        let mut written = 0usize;
        let mut failure = None;
        for subkey in *subkeys {
            let key = format!(r"{subkey}\{HOST_NAME}");
            // Read back rather than trust the write: a registry call that
            // returns success and leaves nothing behind is exactly the bug
            // this is here to catch.
            match set_default_value(&key, &target).and_then(|()| read_default_value(&key)) {
                Ok(back) if back == target => written += 1,
                Ok(back) => failure = Some(format!("{key}: wrote the value and read back {back:?}")),
                Err(e) => failure = Some(format!("{key}: {e}")),
            }
        }
        let result = if written > 0 {
            Ok(format!("{written} of {} hives", subkeys.len()))
        } else {
            Err(failure.unwrap_or_else(|| "no hive could be written".to_string()))
        };
        lines.push((brand.clone(), result));
    }

    Ok(Report { manifest, host_exe: host, lines })
}

/// Remove the per-brand keys, leaving the manifest in place.
pub fn unregister(brands: &[String]) -> Vec<(String, Result<(), String>)> {
    brands
        .iter()
        .filter_map(|brand| {
            let (_, subkeys) = BRANDS.iter().find(|(b, _)| b == brand)?;
            let mut outcome = Ok(());
            for subkey in *subkeys {
                let key = format!(r"{subkey}\{HOST_NAME}");
                if let Err(e) = delete_key(&key) {
                    outcome = Err(e);
                }
            }
            Some((brand.clone(), outcome))
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

/// Read `HKCU\<subkey>`'s default value back.
fn read_default_value(subkey: &str) -> Result<String, String> {
    use windows_sys::Win32::System::Registry::{
        RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_SZ,
    };

    let path = wide(subkey);
    let mut buf = [0u16; 1024];
    let mut len = (buf.len() * 2) as u32;
    // SAFETY: path is NUL-terminated; buf and len describe one buffer, and
    // len is updated to the bytes written.
    let rc = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            path.as_ptr(),
            std::ptr::null(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            buf.as_mut_ptr().cast(),
            &mut len,
        )
    };
    if rc != 0 {
        return Err(format!("cannot read it back (error {rc})"));
    }
    let chars = (len as usize / 2).saturating_sub(1).min(buf.len());
    Ok(String::from_utf16_lossy(&buf[..chars]))
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
    fn every_brand_names_at_least_one_hive_and_none_repeat() {
        let mut all: Vec<&str> = Vec::new();
        for (brand, hives) in BRANDS {
            assert!(!hives.is_empty(), "{brand} names no hive");
            for h in hives {
                assert!(h.ends_with("NativeMessagingHosts"), "{h} is not a host hive");
                all.push(h);
            }
        }
        let before = all.len();
        all.sort_unstable();
        all.dedup();
        // A hive written twice would report two successes for one key.
        assert_eq!(all.len(), before, "a hive is listed under two brands");
    }

    #[test]
    fn brave_covers_its_other_channels() {
        let (_, hives) = BRANDS.iter().find(|(b, _)| *b == "brave").unwrap();
        // A beta or nightly Brave reads its own name, and the symptom of
        // missing it is indistinguishable from no registration at all.
        assert!(hives.iter().any(|h| h.contains("Brave-Browser-Beta")));
        assert!(hives.iter().any(|h| h.contains("Brave-Browser-Nightly")));
        let (_, chrome) = BRANDS.iter().find(|(b, _)| *b == "chrome").unwrap();
        assert!(chrome.iter().any(|h| h.contains(r"Software\Chromium")));
    }

    #[test]
    fn registering_refuses_a_bad_id_before_touching_the_disk() {
        let err = register("nope", &["brave".to_string()]).unwrap_err();
        assert!(err.contains("32 letters"), "{err}");
    }
}
