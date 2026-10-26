use capricorn_core::proto::{Kind, RecordRef};
use clap::{Args, Parser, Subcommand, ValueEnum};

#[derive(Parser, Debug)]
#[command(
    name = "capricorn",
    version,
    about = "Encrypted screen and clipboard memory"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Args, Debug, Clone, Copy)]
pub struct PasswordArgs {
    /// Read the password as one line from stdin instead of prompting on the terminal.
    #[arg(long)]
    pub password_stdin: bool,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Create the vault.
    Init {
        /// Do not create a recovery code.
        #[arg(long)]
        no_recovery: bool,
        #[command(flatten)]
        password: PasswordArgs,
    },
    /// Unlock the vault for this connection.
    Unlock {
        #[command(flatten)]
        password: PasswordArgs,
    },
    /// Lock the vault now.
    Lock,
    /// Show vault and recorder state.
    Status {
        /// Print one Waybar JSON object per state.
        #[arg(long)]
        waybar: bool,
        /// Poll every 2 seconds and print when the state changes.
        #[arg(long)]
        follow: bool,
    },
    /// Search captured screens and clipboard entries.
    Search {
        #[arg(required = true, num_args = 1..)]
        query: Vec<String>,
        /// Only hits from this application class.
        #[arg(long)]
        app: Option<String>,
        #[arg(long, value_enum)]
        kind: Option<KindArg>,
        #[arg(long)]
        json: bool,
        #[arg(long, default_value_t = 20)]
        limit: u32,
        #[command(flatten)]
        password: PasswordArgs,
    },
    /// Delete the last few minutes of captures.
    Forget {
        #[arg(default_value_t = 5)]
        minutes: u32,
    },
    /// Pause recording.
    Pause,
    /// Resume recording.
    Resume,
    /// Change the vault password. With --password-stdin, reads the old then the new password.
    Passwd {
        #[command(flatten)]
        password: PasswordArgs,
    },
    /// Ask the running UI to show a view.
    Show {
        #[arg(value_enum)]
        view: View,
    },
    /// Relaunch the application of a record at its workspace and working directory.
    Reopen {
        /// Record reference as printed by search, like 491000:7.
        #[arg(value_name = "hour:seq")]
        record: RecordRef,
        #[command(flatten)]
        password: PasswordArgs,
    },
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum KindArg {
    Screen,
    Clipboard,
}

impl From<KindArg> for Kind {
    fn from(kind: KindArg) -> Self {
        match kind {
            KindArg::Screen => Kind::Screen,
            KindArg::Clipboard => Kind::Clipboard,
        }
    }
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    Search,
    Timeline,
}

impl View {
    pub fn control_line(self) -> &'static str {
        match self {
            View::Search => "search\n",
            View::Timeline => "timeline\n",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once("capricorn").chain(args.iter().copied()))
    }

    #[test]
    fn parse_ref() {
        let Command::Reopen { record, .. } = parse(&["reopen", "12345:7"]).unwrap().command else {
            panic!("expected reopen");
        };
        assert_eq!(
            record,
            RecordRef {
                hour: 12345,
                seq: 7
            }
        );
    }

    #[test]
    fn parse_ref_rejects_malformed() {
        for bad in ["12345", "a:b", "1:2:3", ":"] {
            let err = parse(&["reopen", bad]).unwrap_err();
            assert!(err.to_string().contains("hour:seq"), "{bad}: {err}");
        }
    }

    #[test]
    fn search_takes_multiple_words_and_defaults() {
        let Command::Search {
            query,
            limit,
            kind,
            json,
            ..
        } = parse(&["search", "quarterly", "report"]).unwrap().command
        else {
            panic!("expected search");
        };
        assert_eq!(query, ["quarterly", "report"]);
        assert_eq!((limit, kind, json), (20, None, false));
    }

    #[test]
    fn search_needs_a_query() {
        assert!(parse(&["search"]).is_err());
    }

    #[test]
    fn forget_defaults_to_five_minutes() {
        let Command::Forget { minutes } = parse(&["forget"]).unwrap().command else {
            panic!("expected forget");
        };
        assert_eq!(minutes, 5);
    }

    #[test]
    fn kind_values_are_lowercase() {
        let Command::Search { kind, .. } = parse(&["search", "x", "--kind", "clipboard"])
            .unwrap()
            .command
        else {
            panic!("expected search");
        };
        assert_eq!(kind.map(Kind::from), Some(Kind::Clipboard));
    }
}
