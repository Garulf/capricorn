use regex::{Regex, RegexBuilder};

/// A window matches a rule when every field the rule sets matches. A rule with no fields set
/// matches nothing.
#[derive(Clone, Debug)]
pub struct Rule {
    pub class: Option<Regex>,
    pub title: Option<Regex>,
    pub workspace: Option<String>,
}

impl Rule {
    fn matches(&self, class: &str, title: &str, workspace: &str) -> bool {
        let has_condition =
            self.class.is_some() || self.title.is_some() || self.workspace.is_some();
        has_condition
            && self.class.as_ref().is_none_or(|re| re.is_match(class))
            && self.title.as_ref().is_none_or(|re| re.is_match(title))
            && self.workspace.as_deref().is_none_or(|w| w == workspace)
    }
}

#[derive(Clone, Debug, Default)]
pub struct RuleSet {
    pub rules: Vec<Rule>,
}

const PASSWORD_MANAGER_CLASSES: &str = r"keepass|bitwarden|1password|proton.?pass|enpass|lastpass|dashlane|nordpass|gnome-keyring|seahorse";
const PRIVATE_WINDOW_TITLES: &str = r"private browsing|incognito|inprivate|private window";

fn case_insensitive(pattern: &str) -> Regex {
    RegexBuilder::new(pattern)
        .case_insensitive(true)
        .build()
        .expect("builtin exclusion pattern compiles")
}

impl RuleSet {
    pub fn defaults() -> Self {
        Self {
            rules: vec![
                Rule {
                    class: Some(case_insensitive(PASSWORD_MANAGER_CLASSES)),
                    title: None,
                    workspace: None,
                },
                Rule {
                    class: None,
                    title: Some(case_insensitive(PRIVATE_WINDOW_TITLES)),
                    workspace: None,
                },
            ],
        }
    }

    pub fn matches(&self, class: &str, title: &str, workspace: &str) -> bool {
        self.rules
            .iter()
            .any(|r| r.matches(class, title, workspace))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_password_managers() {
        let rules = RuleSet::defaults();
        for class in [
            "org.keepassxc.KeePassXC",
            "Bitwarden",
            "1Password",
            "com.bitwarden.desktop",
        ] {
            assert!(rules.matches(class, "Vault", "1"), "{class}");
        }
    }

    #[test]
    fn defaults_match_private_browsing_titles() {
        let rules = RuleSet::defaults();
        for title in [
            "Mozilla Firefox Private Browsing",
            "New Incognito tab - Chromium",
            "Example \u{2014} Private Browsing",
            "Example - Private Browsing",
        ] {
            assert!(rules.matches("firefox", title, "1"), "{title}");
        }
    }

    #[test]
    fn defaults_leave_normal_windows_alone() {
        let rules = RuleSet::defaults();
        assert!(!rules.matches("firefox", "NixOS Wiki", "1"));
        assert!(!rules.matches("kitty", "~/projects", "2"));
    }

    #[test]
    fn rule_fields_are_anded_and_empty_rule_matches_nothing() {
        let rules = RuleSet {
            rules: vec![
                Rule {
                    class: Some(Regex::new("^mpv$").unwrap()),
                    title: None,
                    workspace: Some("7".into()),
                },
                Rule {
                    class: None,
                    title: None,
                    workspace: None,
                },
            ],
        };
        assert!(rules.matches("mpv", "x", "7"));
        assert!(!rules.matches("mpv", "x", "8"));
        assert!(!rules.matches("kitty", "x", "7"));
        assert!(!RuleSet::default().matches("a", "b", "c"));
    }
}
