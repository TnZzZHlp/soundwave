use std::{fmt::Write, net::Ipv4Addr};

/// Encodes public connection information into the canonical V1 pairing URI.
pub fn encode_pairing_uri(host: Ipv4Addr, port: u16, fingerprint: [u8; 32]) -> String {
    let mut fingerprint_text = String::with_capacity(64);
    for byte in fingerprint {
        write!(fingerprint_text, "{byte:02X}").expect("writing to a String cannot fail");
    }

    format!("soundwave://pair/v1?host={host}&port={port}&fp={fingerprint_text}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pairing_uri_is_compact_and_canonical() {
        let uri = encode_pairing_uri(Ipv4Addr::new(192, 168, 1, 42), 48_400, [0xAB; 32]);

        assert_eq!(
            uri,
            "soundwave://pair/v1?host=192.168.1.42&port=48400&fp=ABABABABABABABABABABABABABABABABABABABABABABABABABABABABABABABAB"
        );
    }
}
