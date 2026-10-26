use std::io::{self, BufRead};

use capricorn_core::Secret;

use crate::error::{CliError, Result};

pub struct PasswordInput {
    from_stdin: bool,
}

impl PasswordInput {
    pub fn new(from_stdin: bool) -> Self {
        Self { from_stdin }
    }

    pub fn ask(&self, prompt: &str) -> Result<Secret> {
        if self.from_stdin {
            read_stdin_line()
        } else {
            read_tty(prompt)
        }
    }

    /// On a tty the new password is typed twice; from stdin one line is enough.
    pub fn ask_new(&self, prompt: &str) -> Result<Secret> {
        let first = self.ask(prompt)?;
        if self.from_stdin {
            return Ok(first);
        }
        let again = read_tty("Repeat password: ")?;
        if first != again {
            return Err(CliError::failed("passwords do not match"));
        }
        Ok(first)
    }
}

fn read_tty(prompt: &str) -> Result<Secret> {
    rpassword::prompt_password(prompt)
        .map(Secret::new)
        .map_err(|err| CliError::failed(format!("cannot read password from terminal: {err}")))
}

fn read_stdin_line() -> Result<Secret> {
    let mut line = String::new();
    let read = io::stdin().lock().read_line(&mut line)?;
    if read == 0 {
        return Err(CliError::failed("no password on stdin"));
    }
    Ok(Secret::new(strip_line_ending(line)))
}

fn strip_line_ending(mut line: String) -> String {
    if line.ends_with('\n') {
        line.pop();
        if line.ends_with('\r') {
            line.pop();
        }
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_line_ending_is_stripped() {
        assert_eq!(strip_line_ending("pw \n".into()), "pw ");
        assert_eq!(strip_line_ending("pw\r\n".into()), "pw");
        assert_eq!(strip_line_ending("pw".into()), "pw");
        assert_eq!(strip_line_ending("\n".into()), "");
    }
}
