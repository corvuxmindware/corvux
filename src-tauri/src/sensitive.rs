//! Owned credentials: every clone wipes its own allocation and Debug is redacted.
//! Serde preserves existing wire formats; callers must also protect encoded buffers.

use serde::{Deserialize, Serialize};
use std::{fmt, ops::Deref};
use zeroize::{Zeroize, ZeroizeOnDrop};

#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize, Zeroize, ZeroizeOnDrop)]
#[serde(transparent)]
pub struct SecretString(String);

impl From<String> for SecretString {
    fn from(value: String) -> Self {
        Self(value)
    }
}
impl From<&str> for SecretString {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}
impl Deref for SecretString {
    type Target = str;
    fn deref(&self) -> &str {
        &self.0
    }
}
impl AsRef<str> for SecretString {
    fn as_ref(&self) -> &str {
        &self.0
    }
}
impl fmt::Debug for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[REDACTED]")
    }
}

#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize, Zeroize, ZeroizeOnDrop)]
#[serde(transparent)]
pub struct SecretBytes([u8; 32]);

impl From<[u8; 32]> for SecretBytes {
    fn from(value: [u8; 32]) -> Self {
        Self(value)
    }
}
impl Deref for SecretBytes {
    type Target = [u8; 32];
    fn deref(&self) -> &[u8; 32] {
        &self.0
    }
}
impl AsRef<[u8]> for SecretBytes {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}
impl fmt::Debug for SecretBytes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[REDACTED]")
    }
}

impl PartialEq<str> for SecretString {
    fn eq(&self, other: &str) -> bool {
        self.0 == other
    }
}
impl PartialEq<&str> for SecretString {
    fn eq(&self, other: &&str) -> bool {
        self.0 == *other
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_redact_debug_and_preserve_wire_shape() {
        let text = SecretString::from("credential-sentinel");
        assert_eq!(format!("{text:?}"), "[REDACTED]");
        assert_eq!(
            serde_json::to_string(&text).unwrap(),
            "\"credential-sentinel\""
        );
        let bytes = SecretBytes::from([42; 32]);
        assert_eq!(format!("{bytes:?}"), "[REDACTED]");
        let encoded = postcard::to_allocvec(&bytes).unwrap();
        assert_eq!(encoded, postcard::to_allocvec(&[42u8; 32]).unwrap());
        assert_eq!(
            postcard::from_bytes::<SecretBytes>(&encoded).unwrap(),
            bytes
        );
    }

    #[test]
    fn owned_clones_zeroize_independently() {
        let mut text = SecretString::from("credential-sentinel");
        let cloned = text.clone();
        text.zeroize();
        assert!(text.is_empty());
        assert_eq!(&*cloned, "credential-sentinel");
        let mut bytes = SecretBytes::from([42; 32]);
        let cloned = bytes.clone();
        bytes.zeroize();
        assert_eq!(*bytes, [0; 32]);
        assert_eq!(*cloned, [42; 32]);
        fn wipes_on_drop<T: ZeroizeOnDrop>() {}
        wipes_on_drop::<SecretString>();
        wipes_on_drop::<SecretBytes>();
    }
}
