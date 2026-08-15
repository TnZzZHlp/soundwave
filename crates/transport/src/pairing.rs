use std::{fmt::Write, net::Ipv4Addr};

/// The URI scheme used by QR pairing codes.
pub const PAIRING_URI_SCHEME: &str = "soundwave";
/// The URI authority used by QR pairing codes.
pub const PAIRING_URI_AUTHORITY: &str = "pair";
/// The URI path used by the first pairing-code format.
pub const PAIRING_URI_PATH: &str = "/v1";

/// Immutable, public connection information encoded into a pairing QR code.
///
/// The certificate fingerprint is a pin rather than a secret. Private key
/// material, certificate bytes, and session credentials are never included.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PairingUri {
    host: Ipv4Addr,
    port: u16,
    fingerprint: [u8; 32],
}

impl PairingUri {
    /// Creates a V1 pairing URI for an IPv4 LAN endpoint.
    pub const fn new(host: Ipv4Addr, port: u16, fingerprint: [u8; 32]) -> Self {
        Self {
            host,
            port,
            fingerprint,
        }
    }

    /// Returns the canonical, compact URI encoded in the server QR code.
    pub fn encode(&self) -> String {
        let mut fingerprint = String::with_capacity(64);
        for byte in self.fingerprint {
            write!(fingerprint, "{byte:02X}").expect("writing to a String cannot fail");
        }

        format!(
            "{PAIRING_URI_SCHEME}://{PAIRING_URI_AUTHORITY}{PAIRING_URI_PATH}?host={}&port={}&fp={fingerprint}",
            self.host, self.port,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pairing_uri_is_compact_and_canonical() {
        let uri = PairingUri::new(Ipv4Addr::new(192, 168, 1, 42), 48_400, [0xAB; 32]).encode();

        assert_eq!(
            uri,
            "soundwave://pair/v1?host=192.168.1.42&port=48400&fp=ABABABABABABABABABABABABABABABABABABABABABABABABABABABABABABABAB"
        );
    }
}
