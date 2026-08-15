use qrcodegen::{QrCode, QrCodeEcc};

/// A QR matrix independent of any Windows graphics object.
#[derive(Clone, Debug)]
pub(super) struct QrMatrix {
    size: i32,
    modules: Vec<bool>,
}

impl QrMatrix {
    /// Encodes a compact pairing URI with enough error correction for a desktop display.
    pub(super) fn encode(payload: &str) -> Result<Self, String> {
        let code = QrCode::encode_text(payload, QrCodeEcc::Medium)
            .map_err(|error| format!("QR encoding failed: {error:?}"))?;
        let size = code.size();
        let mut modules = Vec::with_capacity((size * size) as usize);
        for y in 0..size {
            for x in 0..size {
                modules.push(code.get_module(x, y));
            }
        }

        Ok(Self { size, modules })
    }

    pub(super) const fn size(&self) -> i32 {
        self.size
    }

    pub(super) fn is_dark(&self, x: i32, y: i32) -> bool {
        self.modules[(y * self.size + x) as usize]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pairing_payload_encodes_into_a_square_matrix() {
        let matrix = QrMatrix::encode(
            "soundwave://pair/v1?host=192.168.1.42&port=48400&fp=ABABABABABABABABABABABABABABABABABABABABABABABABABABABABABABABAB",
        )
        .unwrap();

        assert!(matrix.size() >= 21);
        assert!(matrix.modules.iter().any(|module| *module));
        assert!(matrix.modules.iter().any(|module| !*module));
    }
}
