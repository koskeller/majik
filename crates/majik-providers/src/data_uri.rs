//! Encoding and decoding of `data:` URIs.

use base64::Engine as _;

pub fn to_data_uri(bytes: &[u8], mime_type: &str) -> String {
    format!("data:{mime_type};base64,{}", base64::engine::general_purpose::STANDARD.encode(bytes))
}

/// Decodes `data:<mime>;base64,<payload>`; returns `None` for anything else.
pub fn from_data_uri(uri: &str) -> Option<Vec<u8>> {
    let (_, payload) = uri.split_once(',')?;
    base64::engine::general_purpose::STANDARD.decode(payload.trim()).ok()
}

pub fn is_data_uri(s: &str) -> bool {
    s.starts_with("data:")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let uri = to_data_uri(b"hello", "text/plain");
        assert_eq!(uri, "data:text/plain;base64,aGVsbG8=");
        assert_eq!(from_data_uri(&uri).unwrap(), b"hello");
        assert!(from_data_uri("nope").is_none());
    }
}
