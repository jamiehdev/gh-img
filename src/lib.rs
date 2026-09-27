pub mod index;
pub mod process;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;

pub use process::{Kind, ProcessError, process, sniff};

pub const MAX_ALT_CHARS: usize = 200;

/// A new stored file name: 128 random bits as 22 base64url characters, then
/// the extension. nginx only serves names of this shape, and issued URLs
/// depend on it, so the format must not change.
pub fn new_name(kind: Kind) -> String {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).expect("OS random source failed");
    format!("{}.{}", URL_SAFE_NO_PAD.encode(bytes), kind.ext())
}

/// Matches `^[A-Za-z0-9_-]{22}\.(png|jpg|gif|webp)$`, the pattern in the
/// nginx vhost. Anything else, including percent-encoded or dotted paths,
/// is rejected before it reaches the filesystem.
pub fn valid_name(name: &str) -> bool {
    let Some((stem, ext)) = name.split_once('.') else {
        return false;
    };

    stem.len() == 22
        && stem
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        && matches!(ext, "png" | "jpg" | "gif" | "webp")
}

/// Make alt text safe inside `![...]`. Brackets and backslashes would end
/// or escape the link text, and control characters would break the line.
pub fn sanitise_alt(raw: &str) -> String {
    let replaced: String = raw
        .chars()
        .map(|c| {
            if matches!(c, '[' | ']' | '\\') || c.is_control() {
                ' '
            } else {
                c
            }
        })
        .collect();
    let collapsed = replaced.split_whitespace().collect::<Vec<_>>().join(" ");
    let capped: String = collapsed.chars().take(MAX_ALT_CHARS).collect();
    let trimmed = capped.trim_end();

    if trimmed.is_empty() {
        "image".to_owned()
    } else {
        trimmed.to_owned()
    }
}

/// Parse a TTL such as `90d`, `12h` or `30m` into seconds.
pub fn parse_ttl(raw: &str) -> Option<u64> {
    let unit = match raw.chars().last()? {
        'd' => 86_400,
        'h' => 3_600,
        'm' => 60,
        _ => return None,
    };
    let n: u64 = raw[..raw.len() - 1].parse().ok()?;

    if n == 0 { None } else { n.checked_mul(unit) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_names_are_valid_and_distinct() {
        let a = new_name(Kind::Png);
        let b = new_name(Kind::Png);
        assert!(valid_name(&a), "{a}");
        assert_ne!(a, b);
        assert!(a.ends_with(".png"));
    }

    #[test]
    fn valid_name_rejects_traversal_and_near_misses() {
        for bad in [
            "../etc/passwd",
            "%2e%2e/x",
            "AAAAAAAAAAAAAAAAAAAAA.png",   // 21 characters
            "AAAAAAAAAAAAAAAAAAAAAAA.png", // 23 characters
            "AAAAAAAAAAAAAAAAAAAAAA.svg",
            "AAAAAAAAAAAAAAAAAAAAAA.png.tmp",
            ".AAAAAAAAAAAAAAAAAAAAA.png",
            "AAAAAAAAAAAAAAAAAAAA/A.png",
            "AAAAAAAAAAAAAAAAAAAAAA.PNG",
        ] {
            assert!(!valid_name(bad), "{bad}");
        }
        assert!(valid_name("wEE9r1x3oM9kp7IeLs6n-_.webp"));
    }

    #[test]
    fn sanitise_alt_cases() {
        assert_eq!(sanitise_alt("a]b"), "a b");
        assert_eq!(sanitise_alt("x\\"), "x");
        assert_eq!(sanitise_alt("bell\u{7}ring"), "bell ring");
        assert_eq!(sanitise_alt("line\r\nbreak"), "line break");
        assert_eq!(sanitise_alt(""), "image");
        assert_eq!(sanitise_alt(" [ ] "), "image");
        assert_eq!(
            sanitise_alt(&"é".repeat(500)).chars().count(),
            MAX_ALT_CHARS
        );
    }

    #[test]
    fn parse_ttl_cases() {
        assert_eq!(parse_ttl("90d"), Some(90 * 86_400));
        assert_eq!(parse_ttl("12h"), Some(12 * 3_600));
        assert_eq!(parse_ttl("30m"), Some(1_800));
        assert_eq!(parse_ttl("0d"), None);
        assert_eq!(parse_ttl("d"), None);
        assert_eq!(parse_ttl("10"), None);
        assert_eq!(parse_ttl("-1d"), None);
        assert_eq!(parse_ttl("99999999999999999999d"), None);
    }
}
