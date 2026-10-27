use capricorn_core::Secret;
use iced::widget::{Space, column, container, row, text, text_input};
use iced::{Alignment, Element, Length};

use crate::icons::{Icon, icon};
use crate::style;

pub const PASSWORD_INPUT: &str = "unlock-password";

/// Why the UI cannot show captures yet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Gate {
    Connecting,
    Unreachable(String),
    Uninitialized,
    Locked,
}

#[derive(Debug)]
pub struct UnlockState {
    password: Secret,
    error: Option<String>,
    busy: bool,
}

#[derive(Clone, Debug)]
pub enum Message {
    PasswordChanged(Secret),
    Submit,
}

impl Default for UnlockState {
    fn default() -> Self {
        Self {
            password: Secret::new(String::new()),
            error: None,
            busy: false,
        }
    }
}

impl UnlockState {
    /// Returns the password to send when the user submits a non-empty field.
    pub fn update(&mut self, message: Message) -> Option<Secret> {
        match message {
            Message::PasswordChanged(password) => {
                self.password = password;
                None
            }
            Message::Submit => {
                if self.busy || self.password.expose().is_empty() {
                    return None;
                }
                self.busy = true;
                self.error = None;
                Some(std::mem::replace(
                    &mut self.password,
                    Secret::new(String::new()),
                ))
            }
        }
    }

    pub fn failed(&mut self, error: String) {
        self.busy = false;
        self.error = Some(error);
    }

    pub fn succeeded(&mut self) {
        self.busy = false;
        self.error = None;
    }

    pub fn header(&self, gate: &Gate) -> Element<'_, Message> {
        let input: Element<'_, Message> = if *gate == Gate::Locked {
            text_input("Vault password", self.password.expose())
                .id(PASSWORD_INPUT)
                .secure(true)
                .on_input(|value| Message::PasswordChanged(Secret::new(value)))
                .on_submit(Message::Submit)
                .size(17)
                .padding([6, 0])
                .style(style::bare_input)
                .into()
        } else {
            text(gate_title(gate))
                .size(17)
                .color(style::TEXT_MUTED)
                .into()
        };
        row![
            icon(Icon::Lock, 18.0, style::TEXT_MUTED),
            container(input).width(Length::Fill),
            locked_label(),
        ]
        .spacing(12)
        .align_y(Alignment::Center)
        .into()
    }

    pub fn body(&self, gate: &Gate) -> Element<'_, Message> {
        let (line, colour) = match (&self.error, gate) {
            (Some(error), Gate::Locked) => (error.clone(), style::DANGER),
            (None, Gate::Locked) if self.busy => ("Unlocking...".to_string(), style::TEXT_MUTED),
            (None, Gate::Locked) => (
                "Captures stay encrypted until you unlock.".to_string(),
                style::TEXT_MUTED,
            ),
            (_, Gate::Unreachable(reason)) => (reason.clone(), style::DANGER),
            (_, Gate::Uninitialized) => (
                "Run `capricorn init` to create the vault.".to_string(),
                style::TEXT_MUTED,
            ),
            (_, Gate::Connecting) => (String::new(), style::TEXT_MUTED),
        };
        column![text(line).size(14).color(colour), Space::new().height(4)]
            .spacing(4)
            .into()
    }
}

fn gate_title(gate: &Gate) -> &'static str {
    match gate {
        Gate::Connecting => "Connecting to capricornd",
        Gate::Unreachable(_) => "capricornd is not running",
        Gate::Uninitialized => "No vault yet",
        Gate::Locked => "",
    }
}

fn locked_label<'a, M: 'a>() -> Element<'a, M> {
    text("locked").size(13).color(style::WARNING).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn submit_hands_over_password_once() {
        let mut state = UnlockState::default();
        assert!(state.update(Message::Submit).is_none());
        state.update(Message::PasswordChanged(Secret::new("pw".into())));
        let sent = state.update(Message::Submit).unwrap();
        assert_eq!(sent.expose(), "pw");
        assert!(state.busy);
        assert!(state.update(Message::Submit).is_none());
        state.failed("wrong password".into());
        assert!(!state.busy);
    }
}
