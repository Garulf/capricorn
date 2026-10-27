use std::fmt;

use serde::{Deserialize, Serialize};

use crate::proto::{Kind, Ts, WindowContext};

/// One stored capture. Its plaintext only ever exists in memory.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Default)]
pub struct Record {
    pub ts: Ts,
    pub kind: Kind,
    pub window: WindowContext,
    pub text: String,
    pub url: Option<String>,
    /// WebP, full resolution.
    pub image: Option<Vec<u8>>,
    /// WebP, 320 px wide.
    pub thumb: Option<Vec<u8>>,
}

impl fmt::Debug for Record {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Record")
            .field("ts", &self.ts)
            .field("kind", &self.kind)
            .field("class", &self.window.class)
            .field("text_len", &self.text.len())
            .field("image_len", &self.image.as_ref().map(Vec::len))
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_roundtrips_through_postcard() {
        let record = Record {
            ts: 1_700_000_000_000,
            kind: Kind::Clipboard,
            text: "nix flake update".into(),
            image: Some(vec![1, 2, 3]),
            ..Record::default()
        };
        let bytes = postcard::to_stdvec(&record).unwrap();
        assert_eq!(postcard::from_bytes::<Record>(&bytes).unwrap(), record);
    }

    #[test]
    fn debug_hides_text() {
        let record = Record {
            text: "very secret".into(),
            ..Record::default()
        };
        assert!(!format!("{record:?}").contains("very secret"));
    }
}
