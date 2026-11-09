//! What browsers this machine has, and what profiles they hold.
//!
//! Lane resolution needs three facts the operator should never have to type:
//! which browser is the default, where each brand keeps its profiles, and what
//! those profiles are called. All three are on disk or in the registry, so
//! this module reads them and nothing else guesses.
//!
//! It lives in the broker rather than in `kratos-protocol`, which has no I/O
//! at all by design, and rather than in the settings window, which gets these
//! facts over the wire instead -- two readers of `Local State` would be two
//! things to keep in step, and the one in the UI process would be reading a
//! machine that is not necessarily the broker's.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

use serde_json::Value;

/// The Chromium family, in the order a search falls back through them.
///
/// Opera is listed because the kill switch and the gate already name it, but
/// its profile layout is not Chromium's -- `Opera Stable` *is* the profile
/// directory, with no `Default` inside it -- so profile discovery for Opera
/// returns the one profile it has and nothing more. Said here because finding
/// out by trying it on a live machine is how an evening goes.
pub const BRANDS: [&str; 5] = ["brave", "chrome", "edge", "vivaldi", "opera"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Browser {
    pub brand: &'static str,
    /// The executable to launch, when one was found.
    pub exe: Option<PathBuf>,
    /// The `User Data` directory that holds the profiles.
    pub user_data: Option<PathBuf>,
}

impl Browser {
    /// Installed enough to be driven: there is something to launch.
    pub fn installed(&self) -> bool {
        self.exe.is_some()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    /// The directory name, as the browser writes it: `Default`, `Profile 2`.
    pub dir: String,
    /// What the browser calls it, from `Local State`'s `info_cache`.
    pub name: Option<String>,
}

fn env_path(var: &str, tail: &str) -> Option<PathBuf> {
    let base = std::env::var_os(var)?;
    if base.is_empty() {
        return None;
    }
    Some(PathBuf::from(base).join(tail))
}

fn first_existing(candidates: &[Option<PathBuf>]) -> Option<PathBuf> {
    candidates.iter().flatten().find(|p| p.exists()).cloned()
}

/// Canonical brand name for a string the user or a config file supplied.
pub fn normalise(brand: &str) -> Option<&'static str> {
    let b = brand.trim().to_ascii_lowercase();
    BRANDS.iter().copied().find(|k| *k == b).or(match b.as_str() {
        // The names people actually type, and the process images.
        "brave.exe" | "brave-browser" => Some("brave"),
        "chrome.exe" | "google chrome" => Some("chrome"),
        "msedge.exe" | "microsoft edge" => Some("edge"),
        "vivaldi.exe" => Some("vivaldi"),
        "opera.exe" | "opera_gx.exe" | "opera gx" => Some("opera"),
        _ => None,
    })
}

/// Where a brand keeps its executable and its profiles on this machine.
///
/// Both halves are looked for in every place that brand is known to install
/// to, per-user and per-machine, because a browser installed for one user is
/// not where the machine-wide installer puts it and a lane that resolves to a
/// path nobody has is worse than one that says "unavailable".
pub fn locate(brand: &str) -> Option<Browser> {
    let brand = normalise(brand)?;
    let (exe, user_data) = match brand {
        "brave" => (
            vec![
                env_path("ProgramFiles", r"BraveSoftware\Brave-Browser\Application\brave.exe"),
                env_path("ProgramFiles(x86)", r"BraveSoftware\Brave-Browser\Application\brave.exe"),
                env_path("LOCALAPPDATA", r"BraveSoftware\Brave-Browser\Application\brave.exe"),
            ],
            vec![env_path("LOCALAPPDATA", r"BraveSoftware\Brave-Browser\User Data")],
        ),
        "chrome" => (
            vec![
                env_path("ProgramFiles", r"Google\Chrome\Application\chrome.exe"),
                env_path("ProgramFiles(x86)", r"Google\Chrome\Application\chrome.exe"),
                env_path("LOCALAPPDATA", r"Google\Chrome\Application\chrome.exe"),
            ],
            vec![env_path("LOCALAPPDATA", r"Google\Chrome\User Data")],
        ),
        "edge" => (
            vec![
                env_path("ProgramFiles(x86)", r"Microsoft\Edge\Application\msedge.exe"),
                env_path("ProgramFiles", r"Microsoft\Edge\Application\msedge.exe"),
            ],
            vec![env_path("LOCALAPPDATA", r"Microsoft\Edge\User Data")],
        ),
        "vivaldi" => (
            vec![
                env_path("LOCALAPPDATA", r"Vivaldi\Application\vivaldi.exe"),
                env_path("ProgramFiles", r"Vivaldi\Application\vivaldi.exe"),
                env_path("ProgramFiles(x86)", r"Vivaldi\Application\vivaldi.exe"),
            ],
            vec![env_path("LOCALAPPDATA", r"Vivaldi\User Data")],
        ),
        "opera" => (
            vec![
                env_path("LOCALAPPDATA", r"Programs\Opera\opera.exe"),
                env_path("ProgramFiles", r"Opera\opera.exe"),
            ],
            vec![env_path("APPDATA", r"Opera Software\Opera Stable")],
        ),
        _ => return None,
    };
    Some(Browser {
        brand,
        exe: first_existing(&exe),
        user_data: first_existing(&user_data),
    })
}

/// Every brand with something to launch, in [`BRANDS`] order.
pub fn installed() -> Vec<Browser> {
    BRANDS.iter().filter_map(|b| locate(b)).filter(Browser::installed).collect()
}

// ---------------------------------------------------------------------------
// Local State
// ---------------------------------------------------------------------------

/// `Local State` is a megabyte of JSON and lane resolution happens on every
/// connection and every `status`, so it is cached and invalidated by
/// modification time.
///
/// Not an optimisation for its own sake: without it, `status` with three
/// sessions attached re-read and re-parsed three browsers' worth of JSON on a
/// call an agent makes between every action.
type StateCache = std::collections::HashMap<String, (std::time::SystemTime, u64, Option<Arc<Value>>)>;

fn cache() -> &'static Mutex<StateCache> {
    static CACHE: OnceLock<Mutex<StateCache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(StateCache::new()))
}

fn local_state(brand: &str) -> Option<Arc<Value>> {
    let path = locate(brand)?.user_data?.join("Local State");
    let meta = std::fs::metadata(&path).ok()?;
    let stamp = (meta.modified().ok()?, meta.len());

    if let Ok(c) = cache().lock() {
        if let Some((seen, size, parsed)) = c.get(brand) {
            if (*seen, *size) == stamp {
                return parsed.clone();
            }
        }
    }

    let parsed = std::fs::read_to_string(&path).ok().and_then(|text| {
        // `str::trim` leaves U+FEFF alone and Chromium has been known to write
        // one, so the BOM is stripped explicitly -- the same byte that broke
        // config loading and the MCP handshake.
        serde_json::from_str::<Value>(text.trim_start_matches('\u{feff}').trim()).ok()
    });
    let parsed = parsed.map(Arc::new);
    if let Ok(mut c) = cache().lock() {
        c.insert(brand.to_string(), (stamp.0, stamp.1, parsed.clone()));
    }
    parsed
}

/// The profiles a brand knows about, by directory name.
///
/// Read from `Local State`'s `info_cache`, which is the same map the browser's
/// own profile picker is drawn from, so the names here are the names the user
/// sees. Sorted with `Default` first and the rest alphabetically, because a
/// dropdown that reorders itself between openings is a dropdown nobody trusts.
pub fn profiles(brand: &str) -> Vec<Profile> {
    let Some(state) = local_state(brand) else {
        return Vec::new();
    };
    let Some(cache) = state.pointer("/profile/info_cache").and_then(|c| c.as_object()) else {
        return Vec::new();
    };
    let mut out: Vec<Profile> = cache
        .iter()
        .map(|(dir, info)| Profile {
            dir: dir.clone(),
            name: info.get("name").and_then(|n| n.as_str()).map(|s| s.to_string()),
        })
        .collect();
    out.sort_by(|a, b| {
        let rank = |p: &Profile| u8::from(p.dir != "Default");
        rank(a).cmp(&rank(b)).then_with(|| a.dir.cmp(&b.dir))
    });
    out
}

/// The profile this brand was last used with, from `profile.last_used`.
///
/// Chromium writes this on every profile switch, so it is the closest thing to
/// "the profile the user means" that exists without asking. A brand that has
/// only ever run one profile does not write it at all, hence the fallback to
/// the only profile there is, and then to `Default`.
pub fn last_used(brand: &str) -> String {
    if let Some(state) = local_state(brand) {
        if let Some(dir) = state
            .pointer("/profile/last_used")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            return dir.to_string();
        }
    }
    match profiles(brand).first() {
        Some(p) => p.dir.clone(),
        None => "Default".to_string(),
    }
}

// ---------------------------------------------------------------------------
// The OS default browser
// ---------------------------------------------------------------------------

/// `https` handler ProgIds, as the installers register them.
fn brand_for_progid(progid: &str) -> Option<&'static str> {
    let p = progid.to_ascii_lowercase();
    // Prefix matching, because the ProgId carries a per-install suffix:
    // `BraveHTML.T3SM4K...` on a machine with more than one channel.
    for (needle, brand) in [
        ("bravehtml", "brave"),
        ("brave", "brave"),
        ("chromehtml", "chrome"),
        ("msedgehtm", "edge"),
        ("msedgedhtml", "edge"),
        ("vivaldihtm", "vivaldi"),
        ("operastable", "opera"),
        ("opera", "opera"),
    ] {
        if p.starts_with(needle) {
            return Some(brand);
        }
    }
    None
}

/// The brand the user's default browser belongs to.
///
/// `UserChoice\ProgId` is the value Windows itself honours when something is
/// opened, so it is the only answer that matches what the user would see if
/// they clicked a link. It is deliberately read-only: nothing here ever sets
/// a default browser.
///
/// `None` when the default is not a Chromium browser this can drive (Firefox,
/// or an app that claims `https`), which is a lane the user has to allocate by
/// hand rather than something to substitute silently.
#[cfg(windows)]
pub fn os_default_brand() -> Option<&'static str> {
    let progid = read_user_choice()?;
    brand_for_progid(&progid)
}

#[cfg(windows)]
fn read_user_choice() -> Option<String> {
    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::System::Registry::{
        RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_SZ,
    };

    let key: Vec<u16> = r"Software\Microsoft\Windows\Shell\Associations\UrlAssociations\https\UserChoice"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let name: Vec<u16> = "ProgId".encode_utf16().chain(std::iter::once(0)).collect();
    let mut buf = [0u16; 256];
    let mut size = (buf.len() * 2) as u32;
    let rc = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            name.as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            buf.as_mut_ptr() as *mut _,
            &mut size,
        )
    };
    if rc != ERROR_SUCCESS {
        return None;
    }
    let chars = (size as usize / 2).min(buf.len());
    let len = buf[..chars].iter().position(|c| *c == 0).unwrap_or(chars);
    Some(String::from_utf16_lossy(&buf[..len]))
}

#[cfg(not(windows))]
pub fn os_default_brand() -> Option<&'static str> {
    None
}

/// The brand to use when nothing has been allocated.
///
/// The OS default first, then the first installed brand, because a machine
/// whose default browser is Firefox still has a Chromium one to drive more
/// often than not -- and `None` here means no lane can be resolved at all,
/// which is a worse answer than "the one that is there".
pub fn fallback_brand() -> Option<&'static str> {
    if let Some(b) = os_default_brand() {
        if locate(b).is_some_and(|x| x.installed()) {
            return Some(b);
        }
    }
    installed().first().map(|b| b.brand)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn brands_normalise_from_what_people_type_and_from_process_images() {
        assert_eq!(normalise("Brave"), Some("brave"));
        assert_eq!(normalise(" vivaldi.exe "), Some("vivaldi"));
        assert_eq!(normalise("Microsoft Edge"), Some("edge"));
        assert_eq!(normalise("firefox"), None);
        assert_eq!(normalise(""), None);
    }

    #[test]
    fn progids_map_to_brands_including_the_suffixed_ones() {
        // A machine with two channels installed gets a per-install suffix, so
        // this has to be a prefix match rather than equality.
        assert_eq!(brand_for_progid("BraveHTML"), Some("brave"));
        assert_eq!(brand_for_progid("BraveHTML.T3SM4K7QWERTY"), Some("brave"));
        assert_eq!(brand_for_progid("ChromeHTML"), Some("chrome"));
        assert_eq!(brand_for_progid("MSEdgeHTM"), Some("edge"));
        assert_eq!(brand_for_progid("VivaldiHTM.3JK"), Some("vivaldi"));
        assert_eq!(brand_for_progid("FirefoxURL-308046B0AF4A39CB"), None);
    }

    #[test]
    fn a_located_brand_reports_paths_that_exist_or_none() {
        // Whatever is on the machine running this, every path reported must be
        // real: a lane that resolves to a path nobody has is worse than one
        // that says unavailable.
        for brand in BRANDS {
            let Some(b) = locate(brand) else { continue };
            assert_eq!(b.brand, brand);
            if let Some(exe) = &b.exe {
                assert!(exe.exists(), "{brand}: {} does not exist", exe.display());
            }
            if let Some(ud) = &b.user_data {
                assert!(ud.exists(), "{brand}: {} does not exist", ud.display());
            }
        }
    }

    #[test]
    fn profiles_put_default_first_and_name_what_the_browser_names() {
        let mut found = 0;
        for brand in BRANDS {
            let ps = profiles(brand);
            if ps.is_empty() {
                continue;
            }
            found += 1;
            if ps.iter().any(|p| p.dir == "Default") {
                assert_eq!(ps[0].dir, "Default", "{brand}: Default must sort first");
            }
            // last_used must be one of them, or the resolver would point at a
            // profile that is not there.
            let lu = last_used(brand);
            assert!(
                ps.iter().any(|p| p.dir == lu),
                "{brand}: last_used {lu:?} is not among {:?}",
                ps.iter().map(|p| &p.dir).collect::<Vec<_>>()
            );
        }
        if found == 0 {
            eprintln!("no Chromium browser on this machine; profile discovery untested");
        }
    }

    #[test]
    fn last_used_always_answers_something() {
        // Even for a brand that is not installed: the resolver has to have a
        // profile name to put in the lane, and `Default` is the one every
        // Chromium creates on first run.
        assert!(!last_used("chrome").is_empty());
        assert_eq!(last_used("firefox"), "Default");
    }

    #[test]
    fn the_fallback_brand_is_installed_if_it_is_anything() {
        match fallback_brand() {
            Some(b) => {
                assert!(locate(b).unwrap().installed(), "{b} was offered and is not installed")
            }
            None => eprintln!("no Chromium browser on this machine"),
        }
    }
}
