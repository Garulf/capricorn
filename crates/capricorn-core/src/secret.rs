use std::fmt;

use serde::{Deserialize, Serialize};
use zeroize::{Zeroize, ZeroizeOnDrop};

/// A password or recovery code. Zeroized on drop; `Debug` never shows the value.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, Zeroize, ZeroizeOnDrop)]
#[serde(transparent)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: String) -> Self {
        Self(value)
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_is_redacted() {
        let secret = Secret::new("hunter2".into());
        assert!(!format!("{secret:?}").contains("hunter2"));
    }

    #[test]
    fn expose_returns_value() {
        assert_eq!(Secret::new("hunter2".into()).expose(), "hunter2");
    }

    #[test]
    fn serde_is_transparent() {
        let bytes = postcard::to_stdvec(&Secret::new("abc".into())).unwrap();
        assert_eq!(bytes, postcard::to_stdvec("abc").unwrap());
        let back: Secret = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(back.expose(), "abc");
    }
}
