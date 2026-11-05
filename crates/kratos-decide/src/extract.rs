//! Pulling literals out of the operator's instruction.
//!
//! The model never produces text. Everything typed, pressed or navigated to
//! comes from here, out of the instruction the operator wrote. That is a
//! deliberate boundary: a decision layer that cannot emit a string cannot
//! invent a recipient, a URL or an amount, however the screen tries to
//! persuade it.
//!
//! The rule for typed text is **quotes or nothing**. An earlier build inferred
//! the payload from trailing words, and `put something sensible in the subject
//! line` was duly parsed as a request to type the characters "something
//! sensible" into a live field. Refusing is the right failure: an agent told
//! to type without quoting gets `refused` and has to say what it means.

/// The literal text to type, taken from the first quoted span.
///
/// Recognises straight and curly quotes. Returns `None` when nothing is
/// quoted, which the caller must treat as a refusal for a `type` action
/// rather than as licence to guess.
pub fn quoted(instruction: &str) -> Option<String> {
    let chars: Vec<char> = instruction.chars().collect();
    let mut i = 0usize;
    while i < chars.len() {
        let closer = match chars[i] {
            '"' => Some('"'),
            '\u{201c}' => Some('\u{201d}'),
            '\u{2018}' => Some('\u{2019}'),
            // A straight apostrophe only opens a quote at a word boundary,
            // so `don't` is not the start of a span.
            '\'' if i == 0 || !chars[i - 1].is_alphanumeric() => Some('\''),
            _ => None,
        };
        if let Some(c) = closer {
            if let Some(end) = (i + 1..chars.len()).find(|&j| {
                chars[j] == c && (c != '\'' || chars.get(j + 1).is_none_or(|n| !n.is_alphanumeric()))
            }) {
                let inner: String = chars[i + 1..end].iter().collect();
                // An empty span is not a payload; keep looking.
                if !inner.is_empty() {
                    return Some(inner);
                }
                i = end + 1;
                continue;
            }
        }
        i += 1;
    }
    None
}

/// Named keys and shortcuts, longest first so `ctrl+shift+n` wins over `n`.
const KEYS: [&str; 26] = [
    "enter", "return", "escape", "esc", "tab", "backspace", "delete", "space", "home", "end",
    "pageup", "pagedown", "up", "down", "left", "right", "insert", "f1", "f2", "f3", "f4", "f5",
    "f6", "f11", "f12", "printscreen",
];

const MODIFIERS: [&str; 5] = ["ctrl", "control", "alt", "shift", "win"];

/// The key or shortcut named in the instruction, normalised to
/// `ctrl+shift+n` form.
pub fn key(instruction: &str) -> Option<String> {
    let lower = instruction.to_lowercase();

    // An explicit combination, written with + or -, takes priority.
    for token in lower.split(|c: char| c.is_whitespace() || "\"'`,;()[]".contains(c)) {
        let t = token.trim_matches(|c: char| !c.is_alphanumeric() && c != '+' && c != '-');
        if t.contains('+') || (t.contains('-') && t.matches('-').count() == 1 && !t.starts_with('-'))
        {
            let parts: Vec<&str> = t.split(['+', '-']).filter(|p| !p.is_empty()).collect();
            if parts.len() >= 2
                && parts[..parts.len() - 1].iter().all(|p| MODIFIERS.contains(p))
                && !parts.last().unwrap().is_empty()
            {
                let mut norm: Vec<String> = parts[..parts.len() - 1]
                    .iter()
                    .map(|m| if *m == "control" { "ctrl".to_string() } else { m.to_string() })
                    .collect();
                norm.push(parts.last().unwrap().to_string());
                return Some(norm.join("+"));
            }
        }
    }

    // Otherwise a bare named key, matched as a whole word.
    let words: Vec<String> = lower
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(|w| w.to_string())
        .collect();
    // "page up" and "page down" are written as two words more often than one.
    for pair in words.windows(2) {
        if pair[0] == "page" && (pair[1] == "up" || pair[1] == "down") {
            return Some(format!("page{}", pair[1]));
        }
    }
    words.iter().find(|w| KEYS.contains(&w.as_str())).map(|w| match w.as_str() {
        "esc" => "escape".to_string(),
        "return" => "enter".to_string(),
        other => other.to_string(),
    })
}

/// A URL named in the instruction, normalised to include a scheme.
pub fn url(instruction: &str) -> Option<String> {
    for raw in instruction.split(|c: char| c.is_whitespace() || "\"'<>`,;()[]".contains(c)) {
        let t = raw.trim_end_matches(['.', ',', '!', '?', ':']);
        if t.is_empty() {
            continue;
        }
        let lower = t.to_lowercase();
        if lower.starts_with("http://") || lower.starts_with("https://") {
            return Some(t.to_string());
        }
        // A bare host: at least one dot, a plausible TLD, no spaces, and not
        // an email address.
        if !t.contains('@') {
            if let Some((host, _)) = t.split_once('/') {
                if looks_like_host(host) {
                    return Some(format!("https://{t}"));
                }
            }
            if looks_like_host(t) {
                return Some(format!("https://{t}"));
            }
        }
    }
    None
}

fn looks_like_host(t: &str) -> bool {
    let Some(dot) = t.rfind('.') else { return false };
    let tld = &t[dot + 1..];
    let stem = &t[..dot];
    tld.len() >= 2
        && tld.chars().all(|c| c.is_ascii_alphabetic())
        && !stem.is_empty()
        && stem.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
}

/// Connectives that may sit between the opener and the name.
const LEADING_FILLER: [&str; 9] = ["the", "a", "an", "my", "to", "up", "on", "over", "into"];

/// Words that end an application name.
const NAME_TERMINATORS: [&str; 12] = [
    "in", "and", "then", "with", "to", "from", "on", "at", "window", "app", "application", "please",
];

/// The application name in an `open` or `focus` instruction.
///
/// `open instagram in brave` yields `instagram`: the terminator list stops the
/// name before the browser, which is a separate target the caller resolves.
pub fn app_name(instruction: &str) -> Option<String> {
    // A quoted name beats any heuristic.
    if let Some(q) = quoted(instruction) {
        return Some(q);
    }
    let words: Vec<&str> = instruction.split_whitespace().collect();
    let openers = ["open", "launch", "start", "run", "switch", "focus", "bring"];
    let mut idx = None;
    for (i, w) in words.iter().enumerate() {
        let clean = w.trim_matches(|c: char| !c.is_alphanumeric()).to_lowercase();
        if openers.contains(&clean.as_str()) {
            idx = Some(i + 1);
            break;
        }
    }
    let start = idx?;
    let mut name: Vec<&str> = Vec::new();
    for w in words.iter().skip(start) {
        let clean = w.trim_matches(|c: char| !c.is_alphanumeric()).to_lowercase();
        if clean.is_empty() {
            continue;
        }
        // Connectives between the opener and the name -- "switch *to* Excel",
        // "bring *up* Notepad" -- are skipped while the name is still empty.
        // Once a name has started the same words end it, which is what stops
        // "open instagram in brave" becoming an app called "instagram in
        // brave".
        if name.is_empty() && LEADING_FILLER.contains(&clean.as_str()) {
            continue;
        }
        if NAME_TERMINATORS.contains(&clean.as_str()) {
            break;
        }
        name.push(w.trim_matches(|c: char| !c.is_alphanumeric() && c != '.' && c != '-'));
        // Application names are short; more than four words means the
        // heuristic has lost the thread.
        if name.len() >= 4 {
            break;
        }
    }
    let joined = name.join(" ").trim().to_string();
    if joined.is_empty() {
        None
    } else {
        Some(joined)
    }
}

/// Scroll direction and magnitude, in wheel ticks. Negative is up.
pub fn scroll_amount(instruction: &str) -> i32 {
    let lower = instruction.to_lowercase();
    let magnitude = lower
        .split(|c: char| !c.is_ascii_digit())
        .filter(|s| !s.is_empty())
        .find_map(|s| s.parse::<i32>().ok())
        .unwrap_or(3)
        .clamp(1, 30);
    let up = lower.contains(" up") || lower.contains("upward") || lower.contains("to the top");
    if up {
        -magnitude
    } else {
        magnitude
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- the dangerous case -------------------------------------------------

    #[test]
    fn unquoted_text_is_never_invented() {
        // Found live: this was parsed as a request to type the characters
        // "something sensible" into a live subject field.
        assert_eq!(quoted("put something sensible in the subject line"), None);
        assert_eq!(quoted("fill in the form appropriately"), None);
        assert_eq!(quoted("write a nice reply"), None);
    }

    #[test]
    fn quoted_text_is_taken_verbatim() {
        assert_eq!(
            quoted("type \"Order-taking AI agents for distributors\" into Subject"),
            Some("Order-taking AI agents for distributors".into())
        );
        assert_eq!(quoted("type 'hello world' there"), Some("hello world".into()));
        assert_eq!(
            quoted("type \u{201c}curly quoted\u{201d} please"),
            Some("curly quoted".into())
        );
    }

    #[test]
    fn an_apostrophe_is_not_an_opening_quote() {
        assert_eq!(quoted("don't type anything"), None);
        assert_eq!(
            quoted("don't change it, type \"ok\" instead"),
            Some("ok".into())
        );
    }

    #[test]
    fn an_empty_quoted_span_is_not_a_payload() {
        assert_eq!(quoted("type \"\" nothing"), None);
        assert_eq!(quoted("type \"\" then \"real\""), Some("real".into()));
    }

    #[test]
    fn text_containing_a_newline_or_punctuation_survives_intact() {
        assert_eq!(
            quoted("type \"Hi,\n\nThanks - see attached.\" in the body"),
            Some("Hi,\n\nThanks - see attached.".into())
        );
    }

    // -- keys ---------------------------------------------------------------

    #[test]
    fn named_keys_and_shortcuts() {
        assert_eq!(key("press Enter"), Some("enter".into()));
        assert_eq!(key("hit Esc to close"), Some("escape".into()));
        assert_eq!(key("press Return"), Some("enter".into()));
        assert_eq!(key("press ctrl+s"), Some("ctrl+s".into()));
        assert_eq!(key("press Ctrl+Shift+N"), Some("ctrl+shift+n".into()));
        assert_eq!(key("press control+a"), Some("ctrl+a".into()));
        assert_eq!(key("press page down"), Some("pagedown".into()));
        assert_eq!(key("press F5"), Some("f5".into()));
    }

    #[test]
    fn a_sentence_with_no_key_yields_none() {
        assert_eq!(key("click the Send button"), None);
        // A hyphenated word must not be read as a shortcut.
        assert_eq!(key("click the order-taking link"), None);
    }

    // -- urls ---------------------------------------------------------------

    #[test]
    fn urls_with_and_without_a_scheme() {
        assert_eq!(
            url("navigate to https://mail.google.com/mail/u/0"),
            Some("https://mail.google.com/mail/u/0".into())
        );
        assert_eq!(url("go to instagram.com"), Some("https://instagram.com".into()));
        assert_eq!(
            url("open gmail.com/mail in brave"),
            Some("https://gmail.com/mail".into())
        );
        assert_eq!(url("go to example.com."), Some("https://example.com".into()));
    }

    #[test]
    fn an_email_address_is_not_a_url() {
        assert_eq!(url("send it to someone@example.com"), None);
    }

    #[test]
    fn plain_prose_has_no_url() {
        assert_eq!(url("click the Send button"), None);
        assert_eq!(url("type \"hello. goodbye\""), None);
    }

    // -- app names ----------------------------------------------------------

    #[test]
    fn app_name_stops_before_the_browser() {
        // The overlapping-kinds bug: "open instagram in brave" must not read
        // as an app called "instagram in brave".
        assert_eq!(app_name("open instagram in brave"), Some("instagram".into()));
        assert_eq!(app_name("open Notepad"), Some("Notepad".into()));
        assert_eq!(app_name("launch the Calculator app"), Some("Calculator".into()));
        assert_eq!(app_name("switch to Excel"), Some("Excel".into()));
    }

    #[test]
    fn a_quoted_app_name_wins_over_the_heuristic() {
        assert_eq!(
            app_name("open \"Visual Studio Code\" now"),
            Some("Visual Studio Code".into())
        );
    }

    #[test]
    fn no_opener_means_no_name() {
        assert_eq!(app_name("click the Send button"), None);
    }

    // -- scroll -------------------------------------------------------------

    #[test]
    fn scroll_direction_and_magnitude() {
        assert_eq!(scroll_amount("scroll down"), 3);
        assert_eq!(scroll_amount("scroll up"), -3);
        assert_eq!(scroll_amount("scroll down 10 notches"), 10);
        assert_eq!(scroll_amount("scroll up 5"), -5);
        assert_eq!(scroll_amount("scroll to the top"), -3);
        // Absurd magnitudes are clamped rather than obeyed.
        assert_eq!(scroll_amount("scroll down 9999"), 30);
    }
}
