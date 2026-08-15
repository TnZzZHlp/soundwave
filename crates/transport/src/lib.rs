//! QUIC helpers shared by the Windows sender and Android native receiver.

mod config;
mod control;
mod datagram;
mod error;
mod pairing;

pub use config::{
    CertificateIdentity, certificate_fingerprint, format_fingerprint, load_or_create_server_config,
    make_pinned_client_config, make_server_config, parse_fingerprint,
};
pub use control::{ControlReader, ControlWriter, MAX_CONTROL_MESSAGE_BYTES};
pub use datagram::{DatagramReceiver, DatagramSender};
pub use error::TransportError;
pub use pairing::{PAIRING_URI_AUTHORITY, PAIRING_URI_PATH, PAIRING_URI_SCHEME, PairingUri};

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use audio_stream_protocol::{AudioPacket, ControlMessage, PROTOCOL_VERSION};
    use quinn::{ClientConfig, Endpoint, crypto::rustls::QuicClientConfig};
    use rustls::{ClientConfig as RustlsClientConfig, RootCertStore, pki_types::CertificateDer};

    use super::*;

    #[tokio::test]
    async fn local_quic_control_and_datagram_round_trip() {
        let (server_config, identity) = make_server_config().unwrap();
        let server = Endpoint::server(server_config, "127.0.0.1:0".parse().unwrap()).unwrap();
        let server_addr = server.local_addr().unwrap();

        let mut roots = RootCertStore::empty();
        roots
            .add(CertificateDer::from(identity.certificate_der))
            .unwrap();
        let rustls = RustlsClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let mut client = Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
        client.set_default_client_config(ClientConfig::new(Arc::new(
            QuicClientConfig::try_from(rustls).unwrap(),
        )));

        let server_task = tokio::spawn(async move {
            let incoming = server.accept().await.expect("client should arrive");
            let connection = incoming.await.expect("server handshake should succeed");
            let (send, recv) = connection
                .accept_bi()
                .await
                .expect("control stream should open");
            let mut reader = ControlReader::new(recv);
            assert_eq!(
                reader.receive().await.unwrap(),
                ControlMessage::Hello {
                    protocol_version: PROTOCOL_VERSION
                }
            );

            let sender = DatagramSender::new(connection).unwrap();
            sender
                .send(&AudioPacket::new(7, 480, vec![0x34, 0x12, 0x78, 0x56]))
                .unwrap();
            assert_eq!(reader.receive().await.unwrap(), ControlMessage::Stop);
            drop(send);
        });

        let connection = client
            .connect(server_addr, "soundwave.local")
            .unwrap()
            .await
            .unwrap();
        let (send, _recv) = connection.open_bi().await.unwrap();
        let mut writer = ControlWriter::new(send);
        writer
            .send(&ControlMessage::Hello {
                protocol_version: PROTOCOL_VERSION,
            })
            .await
            .unwrap();

        let receiver = DatagramReceiver::new(connection).unwrap();
        assert_eq!(
            receiver.receive().await.unwrap(),
            AudioPacket::new(7, 480, vec![0x34, 0x12, 0x78, 0x56])
        );
        writer.send(&ControlMessage::Stop).await.unwrap();
        server_task.await.unwrap();
    }
}
