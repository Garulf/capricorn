use tantivy::schema::Field;
use tantivy::snippet::SnippetGenerator;
use tantivy::tokenizer::TextAnalyzer;

const SNIPPET_CHARS: usize = 150;
const MAX_EDIT_PREFIX_SLACK: usize = 1;
const MIN_FUZZY_LEN: usize = 4;

pub(crate) struct Snippet {
    pub text: String,
    pub highlights: Vec<(u32, u32)>,
}

/// Marks the words of a stored text that the query matched, with the same prefix and
/// one-typo leniency the query itself applies.
pub(crate) struct Highlighter {
    analyzer: TextAnalyzer,
    terms: Vec<String>,
}

impl Highlighter {
    pub fn new(analyzer: TextAnalyzer, terms: Vec<String>) -> Self {
        Self { analyzer, terms }
    }

    pub fn snippet(&self, field: Field, text: &str) -> Option<Snippet> {
        let matched = self.matching_tokens(text);
        if matched.is_empty() {
            return None;
        }
        let terms = matched.into_iter().map(|token| (token, 1.0)).collect();
        let generator = SnippetGenerator::new(terms, self.analyzer.clone(), field, SNIPPET_CHARS);
        let snippet = generator.snippet(text);
        if snippet.highlighted().is_empty() {
            return None;
        }
        Some(Snippet {
            text: snippet.fragment().to_string(),
            highlights: snippet
                .highlighted()
                .iter()
                .map(|range| (range.start as u32, range.end as u32))
                .collect(),
        })
    }

    fn matching_tokens(&self, text: &str) -> Vec<String> {
        let mut analyzer = self.analyzer.clone();
        let mut stream = analyzer.token_stream(text);
        let mut matched = Vec::new();
        while stream.advance() {
            let token = &stream.token().text;
            if self.terms.iter().any(|term| matches_term(term, token)) && !matched.contains(token) {
                matched.push(token.clone());
            }
        }
        matched
    }
}

pub(crate) fn preview(text: &str) -> String {
    let trimmed = text.trim_start();
    let end = trimmed
        .char_indices()
        .nth(SNIPPET_CHARS)
        .map_or(trimmed.len(), |(index, _)| index);
    trimmed[..end].trim_end().to_string()
}

pub(crate) fn fuzzy_distance(term: &str) -> u8 {
    u8::from(term.chars().count() >= MIN_FUZZY_LEN)
}

fn matches_term(term: &str, token: &str) -> bool {
    if token.starts_with(term) {
        return true;
    }
    if fuzzy_distance(term) == 0 {
        return false;
    }
    let term_len = term.chars().count();
    let lengths = term_len.saturating_sub(MAX_EDIT_PREFIX_SLACK)..=term_len + MAX_EDIT_PREFIX_SLACK;
    lengths
        .into_iter()
        .filter_map(|len| prefix_chars(token, len))
        .any(|prefix| within_one_edit(term, prefix))
}

fn prefix_chars(text: &str, chars: usize) -> Option<&str> {
    if chars == 0 {
        return None;
    }
    match text.char_indices().nth(chars) {
        Some((end, _)) => Some(&text[..end]),
        None if text.chars().count() == chars => Some(text),
        None => None,
    }
}

/// Substitution, insertion, deletion or one adjacent swap.
fn within_one_edit(a: &str, b: &str) -> bool {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.len().abs_diff(b.len()) > 1 {
        return false;
    }
    let common = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    if common == a.len() && common == b.len() {
        return true;
    }
    let (rest_a, rest_b) = (&a[common..], &b[common..]);
    match rest_a.len().cmp(&rest_b.len()) {
        std::cmp::Ordering::Equal => {
            rest_a[1..] == rest_b[1..]
                || (rest_a.len() >= 2
                    && rest_a[0] == rest_b[1]
                    && rest_a[1] == rest_b[0]
                    && rest_a[2..] == rest_b[2..])
        }
        std::cmp::Ordering::Less => rest_a == &rest_b[1..],
        std::cmp::Ordering::Greater => &rest_a[1..] == rest_b,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edit_rules() {
        assert!(within_one_edit("flake", "flake"));
        assert!(within_one_edit("flkae", "flake"));
        assert!(within_one_edit("flak", "flake"));
        assert!(within_one_edit("flakes", "flake"));
        assert!(within_one_edit("flaxe", "flake"));
        assert!(!within_one_edit("flxxe", "flake"));
        assert!(!within_one_edit("fla", "flake"));
    }

    #[test]
    fn prefix_and_typo_matching() {
        assert!(matches_term("nixp", "nixpkgs"));
        assert!(matches_term("nixpk", "nixpkgs"));
        assert!(matches_term("nxipkgs", "nixpkgs"));
        assert!(!matches_term("abc", "abd"));
        assert!(!matches_term("zzzz", "nixpkgs"));
    }

    #[test]
    fn preview_cuts_on_char_boundary() {
        let text = "é".repeat(400);
        assert_eq!(preview(&text).chars().count(), SNIPPET_CHARS);
        assert_eq!(preview("  short  "), "short");
    }
}
