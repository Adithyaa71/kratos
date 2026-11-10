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
/// the beta and nightly channels read theirs, and a plain Chromium build reads
/// `Software\Chromium`. Writing only the obvious one produces the worst kind
/// of failure: a registration that works in the browser you tested and
/// silently does not exist in the next one, with nothing to see but
/// "Specified native messaging host not found".
///
/// Writing a key that no browser ever reads costs nothing.
pub const BRANDS: [(&str, &[&str]); 4] = [
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
    // Vivaldi is Chromium but keeps its own hive, which is easy to miss
    // because so much else about it reads from Chrome's.
    (
        "vivaldi",
        &[r"Software\Vivaldi\NativeMessagingHosts", r"Software\Chromium\NativeMessagingHosts"],
    ),
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

/// Where each brand keeps `ExtensionInstallForcelist`, under HKCU.
///
/// This is how `setup-browser.ps1` installs the bridge: packed to a .crx and
/// force-installed by policy, so it loads however the browser is started and
/// needs no flag and no launcher. The ids it installs therefore have to be in
/// the host manifest, and nothing was putting them there.
const FORCELIST_KEYS: [&str; 5] = [
    r"Software\Policies\BraveSoftware\Brave-Browser\ExtensionInstallForcelist",
    r"Software\Policies\Google\Chrome\ExtensionInstallForcelist",
    r"Software\Policies\Microsoft\Edge\ExtensionInstallForcelist",
    r"Software\Policies\Vivaldi\ExtensionInstallForcelist",
    r"Software\Policies\Chromium\ExtensionInstallForcelist",
];

/// Every extension id a policy on this machine force-installs.
///
/// Forcelist entries are numbered string values holding `<id>;<update_url>`.
/// Read by number rather than enumerated because the numbering is the
/// documented shape of the key and twenty is far past any real list; an
/// enumeration would be more code for the same answer.
pub fn forced_ids() -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for key in FORCELIST_KEYS {
        for n in 1..=20u32 {
            let Ok(value) = read_value(key, &n.to_string()) else { continue };
            let id = value.split(';').next().unwrap_or_default().trim().to_lowercase();
            if valid_id(&id) && !out.contains(&id) {
                out.push(id);
            }
        }
    }
    out
}

/// The manifest text for a host path and every id allowed to reach it.
///
/// Hand-rolled rather than serialised so the field order is stable and the
/// file stays diffable; the only values that vary are paths and ids we
/// control.
///
/// **Plural on purpose.** It took one id before, and the Browser tab takes one
/// id, so pressing Register cut the policy-installed extension out of its own
/// manifest -- after which Chromium refused `connectNative` for it and the
/// only thing that worked was loading the unpacked copy by hand.
pub fn manifest_json(ids: &[String], host_exe: &Path) -> String {
    let path = host_exe.display().to_string().replace('\\', "\\\\");
    let origins: Vec<String> =
        ids.iter().map(|id| format!("    \"chrome-extension://{id}/\"")).collect();
    format!(
        "{{\n  \"name\": \"{HOST_NAME}\",\n  \"description\": \"kratos browser bridge\",\n  \
         \"path\": \"{path}\",\n  \"type\": \"stdio\",\n  \"allowed_origins\": [\n{}\n  ]\n}}\n",
        origins.join(",\n")
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
    // Every id that may legitimately reach the host, not just the one that
    // was typed: a policy-installed extension has an id nobody types, and
    // leaving it out is what made the bridge unreachable on this machine
    // until the manifest was repaired by hand.
    let mut ids = vec![id.to_lowercase()];
    for forced in forced_ids() {
        if !ids.contains(&forced) {
            ids.push(forced);
        }
    }

    // Plain UTF-8, no BOM: Chromium's parser rejects one.
    std::fs::write(&manifest, manifest_json(&ids, &host))
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

/// One *named* value, where `read_default_value` beside it reads a key's
/// default.
///
/// `ExtensionInstallForcelist` holds its entries as numbered named values, so
/// reading the policy needs this.
fn read_value(subkey: &str, name: &str) -> Result<String, String> {
    use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_SZ};

    let path = wide(subkey);
    let value = wide(name);
    let mut buf = [0u16; 1024];
    let mut len = (buf.len() * 2) as u32;
    // SAFETY: both strings are NUL-terminated and outlive the call; buf and
    // len describe one buffer, and len is updated to the bytes written.
    let rc = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            path.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            buf.as_mut_ptr().cast(),
            &mut len,
        )
    };
    if rc != 0 {
        return Err(format!("not set (error {rc})"));
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
            &["ldgfddpecnjhdahallkmdlpiglnflige".to_string()],
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
        let json =
            manifest_json(&["abcdefghijklmnopabcdefghijklmnop".to_string()], Path::new("x.exe"));
        let (_, rest) = json.split_once("chrome-extension://").unwrap();
        let (id, _) = rest.split_once('/').unwrap();
        assert_eq!(id, "abcdefghijklmnopabcdefghijklmnop");
    }

    #[test]
    fn every_id_reaches_the_manifest_and_the_json_stays_valid() {
        // The bug this exists for: the manifest held one origin, the policy
        // force-installed a different id, and Chromium refuses connectNative
        // for an origin that is not listed -- so the extension that was
        // actually loaded could never reach the host, and the only thing that
        // worked was loading an unpacked copy by hand.
        let ids = vec![
            "ldgfddpecnjhdahallkmdlpiglnflige".to_string(),
            "eefdlifpdhceoglncjjmgelhobbfdfgp".to_string(),
        ];
        let json = manifest_json(&ids, Path::new(r"D:\Kratos\target\release\kratos-host.exe"));
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
        let origins = parsed["allowed_origins"].as_array().expect("a list");
        assert_eq!(origins.len(), 2, "{json}");
        for id in &ids {
            assert!(
                origins.iter().any(|o| o == &serde_json::json!(format!("chrome-extension://{id}/"))),
                "{id} is not in {json}"
            );
        }
    }

    #[test]
    fn reading_the_forcelist_never_invents_an_id() {
        // It reads whatever this machine has, which a test cannot know, so
        // what it asserts is the shape: every id that comes back is a real
        // Chromium id, and there are no duplicates. An empty answer is
        // correct on a machine with no policy.
        let ids = forced_ids();
        for id in &ids {
            assert!(valid_id(id), "{id:?} is not a Chromium extension id");
        }
        let mut sorted = ids.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), ids.len(), "duplicate ids: {ids:?}");
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
        // Overlap between brands is fine and deliberate: more than one build
        // reads Software\Chromium, and whichever one you register covers the
        // others. What must not happen is a hive repeated *within* a brand,
        // because that would write one key twice and report two successes.
        for (brand, hives) in BRANDS {
            let mut own: Vec<&str> = hives.to_vec();
            let before = own.len();
            own.sort_unstable();
            own.dedup();
            assert_eq!(own.len(), before, "{brand} lists a hive twice");
        }
        assert!(all.len() >= BRANDS.len());
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
        // Vivaldi keeps its own, which is the one it actually reads.
        let (_, vivaldi) = BRANDS.iter().find(|(b, _)| *b == "vivaldi").unwrap();
        assert!(vivaldi.iter().any(|h| h.contains(r"Software\Vivaldi")));
    }

    #[test]
    fn registering_refuses_a_bad_id_before_touching_the_disk() {
        let err = register("nope", &["brave".to_string()]).unwrap_err();
        assert!(err.contains("32 letters"), "{err}");
    }
}
