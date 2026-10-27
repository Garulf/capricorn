use iced::keyboard::key::Named;
use iced::keyboard::{Key, Modifiers};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shortcut {
    Up,
    Down,
    Left,
    Right,
    PrevDay,
    NextDay,
    Close,
    Copy,
    Reopen,
}

pub fn shortcut(key: &Key, modifiers: Modifiers) -> Option<Shortcut> {
    let ctrl = modifiers.control();
    match key.as_ref() {
        Key::Named(Named::Escape) => Some(Shortcut::Close),
        Key::Named(Named::ArrowUp) => Some(Shortcut::Up),
        Key::Named(Named::ArrowDown) => Some(Shortcut::Down),
        Key::Named(Named::ArrowLeft) if ctrl => Some(Shortcut::PrevDay),
        Key::Named(Named::ArrowRight) if ctrl => Some(Shortcut::NextDay),
        Key::Named(Named::ArrowLeft) => Some(Shortcut::Left),
        Key::Named(Named::ArrowRight) => Some(Shortcut::Right),
        Key::Character(c) if ctrl && c.eq_ignore_ascii_case("c") => Some(Shortcut::Copy),
        Key::Character(c) if ctrl && c.eq_ignore_ascii_case("o") => Some(Shortcut::Reopen),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_overlay_and_timeline_keys() {
        let none = Modifiers::empty();
        let ctrl = Modifiers::CTRL;
        assert_eq!(
            shortcut(&Key::Named(Named::Escape), none),
            Some(Shortcut::Close)
        );
        assert_eq!(
            shortcut(&Key::Character("c".into()), ctrl),
            Some(Shortcut::Copy)
        );
        assert_eq!(
            shortcut(&Key::Character("O".into()), ctrl),
            Some(Shortcut::Reopen)
        );
        assert_eq!(shortcut(&Key::Character("c".into()), none), None);
        assert_eq!(
            shortcut(&Key::Named(Named::ArrowLeft), ctrl),
            Some(Shortcut::PrevDay)
        );
        assert_eq!(
            shortcut(&Key::Named(Named::ArrowRight), none),
            Some(Shortcut::Right)
        );
    }
}
