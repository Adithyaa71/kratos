//! The API key at rest.
//!
//! Stored DPAPI-sealed and base64'd in `config.json`, scoped to the current
//! user, so the file is not a plaintext credential and a copy of it is useless
//! on another machine or under another account.
//!
//! The key exists in exactly two places at runtime: inside this process, and
//! inside the decision provider it is handed to. It is never sent over the
//! pipe in either direction, never returned by `config.get`, and never written
//! to the journal.

use base64::Engine;

fn b64() -> base64::engine::general_purpose::GeneralPurpose {
    base64::engine::general_purpose::STANDARD
}

/// Seal a plaintext key for storage. An empty key stores as empty rather than
/// as an encrypted empty string, so "no key configured" stays obvious.
pub fn seal_key(plain: &str) -> Result<String, String> {
    let t = plain.trim();
    if t.is_empty() {
        return Ok(String::new());
    }
    kratos_ipc::win::protect(t.as_bytes())
        .map(|sealed| b64().encode(sealed))
        .map_err(|e| format!("could not seal the key: {e}"))
}

/// Recover the plaintext key, or an empty string if there is none to recover.
///
/// Falls back to the environment when nothing is sealed, which is what lets an
/// existing `.env`-based setup work on first run without retyping the key into
/// the settings window. A sealed key always wins: the environment is a
/// convenience, not an override.
pub fn read_key(sealed_b64: &str) -> String {
    let t = sealed_b64.trim();
    if !t.is_empty() {
        if let Ok(bytes) = b64().decode(t) {
            if let Ok(plain) = kratos_ipc::win::unprotect(&bytes) {
                if let Ok(s) = String::from_utf8(plain) {
                    return s.trim().to_string();
                }
            }
        }
        // A sealed blob that will not open is worth reporting: it usually
        // means the config was copied from another machine or account.
        eprintln!(
            "kratos-broker: the stored decider key could not be decrypted on this account; \
             falling back to the environment"
        );
    }
    for var in ["KRATOS_API_KEY", "TYPESAFE_API_KEY", "JEV_API_KEY"] {
        if let Ok(v) = std::env::var(var) {
            if !v.trim().is_empty() {
                return v.trim().to_string();
            }
        }
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_key_round_trips_through_dpapi_and_base64() {
        let sealed = seal_key("apikey_example_0123456789").unwrap();
        assert!(!sealed.is_empty());
        // The stored form must not contain the key.
        assert!(!sealed.contains("apikey_example"));
        assert_eq!(read_key(&sealed), "apikey_example_0123456789");
    }

    #[test]
    fn whitespace_is_trimmed_on_the_way_in_and_out() {
        let sealed = seal_key("  apikey_padded  ").unwrap();
        assert_eq!(read_key(&sealed), "apikey_padded");
    }

    #[test]
    fn an_empty_key_stays_empty_rather_than_becoming_a_blob() {
        assert_eq!(seal_key("").unwrap(), "");
        assert_eq!(seal_key("   ").unwrap(), "");
    }

    #[test]
    fn two_seals_of_the_same_key_differ_but_both_open() {
        let a = seal_key("same-key").unwrap();
        let b = seal_key("same-key").unwrap();
        assert_ne!(a, b, "DPAPI should not produce a deterministic blob");
        assert_eq!(read_key(&a), read_key(&b));
    }

    #[test]
    fn garbage_falls_back_to_the_environment_rather_than_panicking() {
        std::env::set_var("KRATOS_API_KEY", "from-env");
        assert_eq!(read_key("this is not base64 or a blob !!!"), "from-env");
        assert_eq!(read_key(""), "from-env");
        std::env::remove_var("KRATOS_API_KEY");
    }

    #[test]
    fn a_sealed_key_beats_the_environment() {
        std::env::set_var("KRATOS_API_KEY", "from-env");
        let sealed = seal_key("from-config").unwrap();
        assert_eq!(read_key(&sealed), "from-config");
        std::env::remove_var("KRATOS_API_KEY");
    }

    #[test]
    fn with_nothing_anywhere_the_key_is_empty_not_a_panic() {
        for v in ["KRATOS_API_KEY", "TYPESAFE_API_KEY", "JEV_API_KEY"] {
            std::env::remove_var(v);
        }
        assert_eq!(read_key(""), "");
    }
}
