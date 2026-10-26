//! A newtype that keeps secrets out of `Debug` output and logs.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A value that must never be printed. `Debug` is redacted; serde is
/// transparent so it can live inside stored sessions and wire structs.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret<T = String>(T);

impl<T> Secret<T> {
    /// Wraps a value.
    pub fn new(v: T) -> Self {
        Secret(v)
    }

    /// Returns the wrapped value. Call sites are where secrets leave the type,
    /// so keep them few and obvious.
    pub fn expose(&self) -> &T {
        &self.0
    }
}

impl<T> fmt::Debug for Secret<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

impl<T: Serialize> Serialize for Secret<T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        self.0.serialize(serializer)
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Secret<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        T::deserialize(deserializer).map(Secret)
    }
}

#[cfg(test)]
mod tests {
    use super::Secret;

    #[test]
    fn debug_is_redacted() {
        let s = Secret::new("hunter2".to_string());
        assert_eq!(format!("{s:?}"), "Secret(<redacted>)");
        assert_eq!(s.expose(), "hunter2");
    }

    #[test]
    fn serde_is_transparent() {
        let s = Secret::new("tok".to_string());
        assert_eq!(serde_json::to_string(&s).unwrap(), "\"tok\"");
        let back: Secret<String> = serde_json::from_str("\"tok\"").unwrap();
        assert_eq!(back, s);
    }
}
