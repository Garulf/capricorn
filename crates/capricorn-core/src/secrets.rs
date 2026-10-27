use std::ops::Range;
use std::sync::LazyLock;

use regex::Regex;

const BUILTIN_PATTERNS: &[&str] = &[
    r"\bgh[pousr]_[A-Za-z0-9]{36,}",
    r"\bgithub_pat_[A-Za-z0-9_]{22,}",
    r"\bsk-(?:proj-|ant-)[A-Za-z0-9_-]{20,}",
    r"\bsk-[A-Za-z0-9]{32,}",
    r"\bxox[abprs]-[A-Za-z0-9-]{5,}",
    r"\b(?:AKIA|ASIA)[0-9A-Z]{16}\b",
    r"\bAIza[0-9A-Za-z_-]{35}",
    r"\beyJ[A-Za-z0-9_-]{5,}\.[A-Za-z0-9_-]{5,}\.[A-Za-z0-9_-]*",
    r"(?s)-----BEGIN [A-Z ]*PRIVATE KEY(?: BLOCK)?-----.*?(?:-----END [A-Z ]*PRIVATE KEY(?: BLOCK)?-----|\z)",
];

static BUILTIN: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    BUILTIN_PATTERNS
        .iter()
        .map(|p| Regex::new(p).expect("builtin secret pattern compiles"))
        .collect()
});

/// Byte ranges of secrets in `text`, sorted, with overlapping matches merged.
pub fn find_secrets(text: &str, extra: &[Regex]) -> Vec<Range<usize>> {
    let mut ranges: Vec<Range<usize>> = BUILTIN
        .iter()
        .chain(extra)
        .flat_map(|re| re.find_iter(text).map(|m| m.range()))
        .collect();
    ranges.sort_by_key(|r| (r.start, r.end));
    merge_overlapping(ranges)
}

pub fn is_secret(text: &str, extra: &[Regex]) -> bool {
    BUILTIN.iter().chain(extra).any(|re| re.is_match(text))
}

fn merge_overlapping(sorted: Vec<Range<usize>>) -> Vec<Range<usize>> {
    let mut merged: Vec<Range<usize>> = Vec::with_capacity(sorted.len());
    for range in sorted {
        match merged.last_mut() {
            Some(last) if range.start <= last.end => last.end = last.end.max(range.end),
            _ => merged.push(range),
        }
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jwt() -> String {
        [
            "eyJhbGciOiJIUzI1NiJ9",
            "eyJzdWIiOiIxMjM0NTY3ODkwIn0",
            "c2lnbmF0dXJlYnl0ZXM",
        ]
        .join(".")
    }

    fn begin(label: &str) -> String {
        format!("-----{}-----", ["BEGIN", label].join(" "))
    }

    fn end(label: &str) -> String {
        format!("-----{}-----", ["END", label].join(" "))
    }

    fn alnum(n: usize) -> String {
        "aB3dE5gH7jK9mN1pQ3sT5vW7yZ9bD2fG4hJ6kL8nP0rS"
            .chars()
            .cycle()
            .take(n)
            .collect()
    }

    #[test]
    fn is_secret_detects_known_tokens() {
        let samples = [
            format!("ghp_{}", alnum(36)),
            format!("github_pat_11ABCDEFG0{}", alnum(40)),
            format!("sk-proj-{}", alnum(40)),
            format!("sk-ant-{}", alnum(40)),
            "xoxb-1-2-abc".to_string(),
            "AKIAIOSFODNN7EXAMPLE".to_string(),
            jwt(),
            begin("OPENSSH PRIVATE KEY"),
            begin("PGP PRIVATE KEY BLOCK"),
        ];
        for sample in samples {
            assert!(is_secret(&sample, &[]), "{sample}");
        }
    }

    #[test]
    fn is_secret_ignores_ordinary_text() {
        for sample in [
            "nix flake update",
            "https://github.com/Garulf/capricorn",
            "sk-",
            "-----BEGIN CERTIFICATE-----",
        ] {
            assert!(!is_secret(sample, &[]), "{sample}");
        }
    }

    #[test]
    fn find_secrets_returns_token_range() {
        let token = format!("ghp_{}", alnum(36));
        let text = format!("export TOKEN={token} && run");
        let found = find_secrets(&text, &[]);
        assert_eq!(found.len(), 1);
        assert_eq!(&text[found[0].clone()], token);
    }

    #[test]
    fn find_secrets_covers_whole_private_key_block() {
        let key = format!(
            "{}\nMIIB\n{}",
            begin("RSA PRIVATE KEY"),
            end("RSA PRIVATE KEY")
        );
        let text = format!("before\n{key}\nafter");
        let found = find_secrets(&text, &[]);
        assert_eq!(found.len(), 1);
        assert_eq!(&text[found[0].clone()], key);
    }

    #[test]
    fn find_secrets_merges_overlaps_and_uses_extra_patterns() {
        let extra = [Regex::new(r"hunter\d").unwrap()];
        let jwt = jwt();
        let text = format!("hunter2 and {jwt}");
        let found = find_secrets(&text, &extra);
        assert_eq!(found.len(), 2);
        assert_eq!(&text[found[0].clone()], "hunter2");
        assert!(is_secret("hunter2", &extra));
        assert!(!is_secret("hunter2", &[]));
    }
}
