package com.example.audiostream

import java.util.Locale

/** Connection data decoded from a `soundwave://pair/v1?` pairing payload. */
internal data class PairingDetails(
    val host: String,
    val port: Int,
    val fingerprint: String,
)

/** Strict parser for the pairing payload embedded in the server QR code. */
internal object PairingPayload {
    private const val URI_PREFIX = "soundwave://pair/v1?"
    private const val MAX_PAYLOAD_LENGTH = 512
    private const val FIRST_PRINTABLE_ASCII = 0x21
    private const val LAST_PRINTABLE_ASCII = 0x7E
    private const val FINGERPRINT_HEX_LENGTH = 64

    fun parse(payload: String): PairingDetails? {
        if (payload.length > MAX_PAYLOAD_LENGTH || payload.any { it.code !in FIRST_PRINTABLE_ASCII..LAST_PRINTABLE_ASCII }) {
            return null
        }
        if (!payload.startsWith(URI_PREFIX)) {
            return null
        }

        val query = payload.removePrefix(URI_PREFIX)
        if (query.count { it == '&' } != 2) {
            return null
        }
        val parameters = query.split("&")
        if (parameters.size != 3 || !parameters[0].startsWith("host=") || !parameters[1].startsWith("port=") || !parameters[2].startsWith("fp=")) {
            return null
        }
        val host = parameters[0].removePrefix("host=")
        val portText = parameters[1].removePrefix("port=")
        val compactFingerprint = parameters[2].removePrefix("fp=")
        val port = portText.toIntOrNull()
        if (!isValidHost(host)
            || !isCanonicalPort(portText)
            || port == null
            || port !in 1..65_535
            || !isValidCompactFingerprint(compactFingerprint)
        ) {
            return null
        }

        return PairingDetails(
            host = host,
            port = port,
            fingerprint = compactFingerprint
                .uppercase(Locale.ROOT)
                .chunked(2)
                .joinToString(":"),
        )
    }

    fun isValidFingerprint(fingerprint: String): Boolean {
        val compact = fingerprint.filterNot { it == ':' || it == '-' || it.isWhitespace() }
        return isValidCompactFingerprint(compact)
    }

    private fun isValidHost(host: String): Boolean {
        val octets = host.split(".")
        if (octets.size != 4) {
            return false
        }
        val values = octets.map { octet ->
            if (octet.isEmpty()
                || octet.length > 3
                || (octet.length > 1 && octet.startsWith("0"))
                || octet.any { it !in '0'..'9' }
            ) {
                return false
            }
            octet.toIntOrNull()?.takeIf { it in 0..255 } ?: return false
        }

        val first = values[0]
        return first != 0
            && first != 127
            && first < 224
            && !(first == 169 && values[1] == 254)
    }

    private fun isCanonicalPort(port: String): Boolean {
        return port.isNotEmpty()
            && port.all { it in '0'..'9' }
            && (port.length == 1 || !port.startsWith("0"))
    }

    private fun isValidCompactFingerprint(fingerprint: String): Boolean {
        return fingerprint.length == FINGERPRINT_HEX_LENGTH
            && fingerprint.all { it in '0'..'9' || it in 'A'..'F' || it in 'a'..'f' }
    }
}
