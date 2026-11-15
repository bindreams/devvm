//! The Windows product key of a recipe's ISO.

use secrecy::{ExposeSecret, SecretString};

/// A Windows product key: five groups of five ASCII letters and digits, such as `XXXXX-XXXXX-XXXXX-XXXXX-XXXXX`.
///
/// The key is a secret: it has no `Display`, and its `Debug` output hides it. [`ProductKey::expose`] is the one way to
/// read it, for the answer file and the layer key.
#[derive(Clone, Debug)]
pub struct ProductKey(SecretString);

impl ProductKey {
    const GROUPS: usize = 5;
    const GROUP_LEN: usize = 5;

    /// The key `text` spells, or `None` when it is not five groups of five letters and digits joined by `-`.
    pub fn parse(text: &str) -> Option<ProductKey> {
        let groups: Vec<&str> = text.split('-').collect();
        let shaped = groups.len() == Self::GROUPS
            && groups
                .iter()
                .all(|group| group.len() == Self::GROUP_LEN && group.bytes().all(|b| b.is_ascii_alphanumeric()));
        shaped.then(|| ProductKey(SecretString::from(text)))
    }

    pub(crate) fn expose(&self) -> &str {
        self.0.expose_secret()
    }
}

impl PartialEq for ProductKey {
    fn eq(&self, other: &ProductKey) -> bool {
        self.expose() == other.expose()
    }
}

impl Eq for ProductKey {}
