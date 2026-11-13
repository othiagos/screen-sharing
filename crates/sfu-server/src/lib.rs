pub mod handler;
pub mod state;
pub mod tls;

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Result;
use quinn::Endpoint;
use tokio::task::JoinHandle;
use tracing::info;

use crate::{
    handler::handle_quic_connection,
    state::SfuState,
    tls::{prepare_server_tls, TlsSource},
};

#[derive(Debug, Clone)]
pub struct SfuServerConfig {
    pub bind_addr: SocketAddr,
    pub auth_token: Option<String>,
    pub tls: TlsSource,
}

pub async fn spawn_sfu_server(
    config: SfuServerConfig,
) -> Result<(SocketAddr, String, Arc<SfuState>, JoinHandle<()>)> {
    let prepared = prepare_server_tls(&config.tls)?;
    let fingerprint = prepared.fingerprint_sha256.clone();
    let endpoint = Endpoint::server(prepared.server_config, config.bind_addr)?;
    let local_addr = endpoint.local_addr()?;
    let state = SfuState::new(config.auth_token);
    let loop_state = Arc::clone(&state);

    let handle = tokio::spawn(async move {
        while let Some(incoming) = endpoint.accept().await {
            let state_conn = Arc::clone(&loop_state);
            tokio::spawn(async move {
                match incoming.await {
                    Ok(conn) => handle_quic_connection(conn, state_conn).await,
                    Err(err) => tracing::debug!("Incoming QUIC handshake failed: {err}"),
                }
            });
        }
    });

    info!(%local_addr, %fingerprint, "QUIC SFU server listening");
    Ok((local_addr, fingerprint, state, handle))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
    use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
    use rustls::{DigitallySignedStruct, SignatureScheme};
    use shared_protocol::{
        decode_control_payload, encode_audio_packet, encode_control_frame, encode_video_packet,
        format_cert_sha256_fingerprint, normalize_fingerprint, AudioPacketHeader, ClientControl,
        ControlErrorCode, PeerId, ServerControl, StreamMetadata, VideoFrameHeader,
        ALPN_SFU_QUIC, AUDIO_FLAG_OPUS, MAX_CONTROL_FRAME_BYTES, PROTOCOL_VERSION,
        VIDEO_FLAG_H264, VIDEO_FLAG_KEYFRAME,
    };
    use std::time::Duration;

    #[derive(Debug)]
    struct FingerprintVerifier {
        expected_norm: String,
    }

    impl ServerCertVerifier for FingerprintVerifier {
        fn verify_server_cert(
            &self,
            end_entity: &CertificateDer<'_>,
            _intermediates: &[CertificateDer<'_>],
            _server_name: &ServerName<'_>,
            _ocsp: &[u8],
            _now: UnixTime,
        ) -> std::result::Result<ServerCertVerified, rustls::Error> {
            let actual = normalize_fingerprint(&format_cert_sha256_fingerprint(end_entity.as_ref()));
            if actual == self.expected_norm {
                Ok(ServerCertVerified::assertion())
            } else {
                Err(rustls::Error::General(format!(
                    "Fingerprint mismatch: expected {}, got {actual}",
                    self.expected_norm
                )))
            }
        }

        fn verify_tls12_signature(
            &self,
            _message: &[u8],
            _cert: &CertificateDer<'_>,
            _dss: &DigitallySignedStruct,
        ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
            Ok(HandshakeSignatureValid::assertion())
        }

        fn verify_tls13_signature(
            &self,
            _message: &[u8],
            _cert: &CertificateDer<'_>,
            _dss: &DigitallySignedStruct,
        ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
            Ok(HandshakeSignatureValid::assertion())
        }

        fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
            rustls::crypto::ring::default_provider()
                .signature_verification_algorithms
                .supported_schemes()
        }
    }

    async fn connect_test_client(
        addr: SocketAddr,
        fingerprint: &str,
        nickname: &str,
        auth_token: Option<&str>,
    ) -> (
        quinn::Endpoint,
        quinn::Connection,
        quinn::SendStream,
        quinn::RecvStream,
        PeerId,
    ) {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let verifier = Arc::new(FingerprintVerifier {
            expected_norm: normalize_fingerprint(fingerprint),
        });
        let mut crypto = rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(verifier)
            .with_no_client_auth();
        crypto.alpn_protocols = vec![ALPN_SFU_QUIC.to_vec()];

        let client_cfg = quinn::ClientConfig::new(Arc::new(
            quinn::crypto::rustls::QuicClientConfig::try_from(crypto).unwrap(),
        ));
        let mut endpoint =
            quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).expect("client endpoint");
        endpoint.set_default_client_config(client_cfg);

        let conn = endpoint
            .connect(addr, "localhost")
            .unwrap()
            .await
            .expect("QUIC connect");
        let (mut send, mut recv) = conn.open_bi().await.expect("open_bi");

        let hello = ClientControl::Hello {
            protocol_version: PROTOCOL_VERSION,
            nickname: nickname.to_string(),
            auth_token: auth_token.map(|s| s.to_string()),
        };
        send.write_all(&encode_control_frame(&hello).unwrap())
            .await
            .unwrap();

        let peer_id = match read_srv(&mut recv).await {
            ServerControl::Welcome { local_peer_id } => local_peer_id,
            other => panic!("Expected Welcome, got {other:?}"),
        };

        (endpoint, conn, send, recv, peer_id)
    }

    async fn send_cli(send: &mut quinn::SendStream, msg: &ClientControl) {
        send.write_all(&encode_control_frame(msg).unwrap())
            .await
            .unwrap();
    }

    async fn read_srv(recv: &mut quinn::RecvStream) -> ServerControl {
        let mut len_buf = [0u8; 4];
        tokio::time::timeout(Duration::from_secs(5), recv.read_exact(&mut len_buf))
            .await
            .expect("timeout waiting server control len")
            .expect("read server len");
        let len = u32::from_le_bytes(len_buf) as usize;
        assert!(len <= MAX_CONTROL_FRAME_BYTES);
        let mut payload = vec![0u8; len];
        recv.read_exact(&mut payload).await.expect("read payload");
        decode_control_payload(&payload).expect("decode ServerControl")
    }

    #[tokio::test]
    async fn test_sfu_auth_password_selective_forward_and_cleanup() {
        let config = SfuServerConfig {
            bind_addr: "127.0.0.1:0".parse().unwrap(),
            auth_token: Some("super-secret".to_string()),
            tls: TlsSource::SelfSigned {
                cert_dir: None,
                extra_sans: vec![],
            },
        };
        let (addr, fp, state, handle) = spawn_sfu_server(config).await.expect("spawn SFU");

        // 1. Unauthorized token must fail
        {
            let _ = rustls::crypto::ring::default_provider().install_default();
            let verifier = Arc::new(FingerprintVerifier {
                expected_norm: normalize_fingerprint(&fp),
            });
            let mut crypto = rustls::ClientConfig::builder()
                .dangerous()
                .with_custom_certificate_verifier(verifier)
                .with_no_client_auth();
            crypto.alpn_protocols = vec![ALPN_SFU_QUIC.to_vec()];
            let mut ep = quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
            ep.set_default_client_config(quinn::ClientConfig::new(Arc::new(
                quinn::crypto::rustls::QuicClientConfig::try_from(crypto).unwrap(),
            )));
            let conn = ep.connect(addr, "localhost").unwrap().await.unwrap();
            let (mut s, mut r) = conn.open_bi().await.unwrap();
            send_cli(
                &mut s,
                &ClientControl::Hello {
                    protocol_version: PROTOCOL_VERSION,
                    nickname: "Intruder".to_string(),
                    auth_token: Some("wrong".to_string()),
                },
            )
            .await;
            match read_srv(&mut r).await {
                ServerControl::Error {
                    code: ControlErrorCode::Unauthorized,
                    ..
                } => {}
                other => panic!("Expected Unauthorized, got {other:?}"),
            }
            ep.close(0u32.into(), b"done");
        }

        // 2. Peer A creates password-protected room; Peer B & C join
        let (ep_a, conn_a, mut send_a, mut recv_a, id_a) =
            connect_test_client(addr, &fp, "Alice", Some("super-secret")).await;
        send_cli(
            &mut send_a,
            &ClientControl::CreateRoom {
                password: Some("roompass".to_string()),
            },
        )
        .await;
        let room_id = match read_srv(&mut recv_a).await {
            ServerControl::RoomJoined { room_id, .. } => room_id,
            other => panic!("Expected RoomJoined for A, got {other:?}"),
        };

        let (ep_b, conn_b, mut send_b, mut recv_b, id_b) =
            connect_test_client(addr, &fp, "Bob", Some("super-secret")).await;
        // Wrong password check
        send_cli(
            &mut send_b,
            &ClientControl::JoinRoom {
                room_id: room_id.clone(),
                password: Some("bad".to_string()),
            },
        )
        .await;
        assert!(matches!(
            read_srv(&mut recv_b).await,
            ServerControl::Error {
                code: ControlErrorCode::InvalidPassword,
                ..
            }
        ));

        send_cli(
            &mut send_b,
            &ClientControl::JoinRoom {
                room_id: room_id.clone(),
                password: Some("roompass".to_string()),
            },
        )
        .await;
        assert!(matches!(
            read_srv(&mut recv_b).await,
            ServerControl::RoomJoined { .. }
        ));
        assert!(matches!(
            read_srv(&mut recv_a).await,
            ServerControl::PeerJoined { .. }
        ));

        let (ep_c, conn_c, mut send_c, mut recv_c, _id_c) =
            connect_test_client(addr, &fp, "Carol", Some("super-secret")).await;
        send_cli(
            &mut send_c,
            &ClientControl::JoinRoom {
                room_id: room_id.clone(),
                password: Some("roompass".to_string()),
            },
        )
        .await;
        assert!(matches!(
            read_srv(&mut recv_c).await,
            ServerControl::RoomJoined { .. }
        ));
        let _ = read_srv(&mut recv_a).await;
        let _ = read_srv(&mut recv_b).await;

        // 3. Alice starts broadcasting; ONLY Bob subscribes (Carol does NOT)
        send_cli(
            &mut send_a,
            &ClientControl::StartBroadcast {
                metadata: StreamMetadata {
                    width: 1920,
                    height: 1080,
                    fps: 60,
                    has_audio: true,
                    codec_name: "H.264".to_string(),
                },
            },
        )
        .await;
        assert!(matches!(
            read_srv(&mut recv_b).await,
            ServerControl::PeerStreamStarted { peer_id, .. } if peer_id == id_a
        ));
        assert!(matches!(
            read_srv(&mut recv_c).await,
            ServerControl::PeerStreamStarted { peer_id, .. } if peer_id == id_a
        ));

        send_cli(
            &mut send_b,
            &ClientControl::SetSubscription {
                broadcaster_id: id_a,
                video: true,
                audio: true,
            },
        )
        .await;

        // Alice receives consolidated ForceKeyframe
        assert!(matches!(
            read_srv(&mut recv_a).await,
            ServerControl::ForceKeyframe
        ));

        // 4. Alice sends a P-frame BEFORE IDR (must be dropped for Bob since awaiting_keyframe=true),
        // then sends an IDR keyframe + audio packet -> Bob receives IDR & audio; Carol receives NOTHING!
        let p_pkt = encode_video_packet(
            &VideoFrameHeader {
                sender_id: id_a,
                frame_seq: 1,
                timestamp_us: 1000,
                width: 1920,
                height: 1080,
                flags: VIDEO_FLAG_H264,
            },
            b"p-frame",
        )
        .unwrap();
        let mut s_p = conn_a.open_uni().await.unwrap();
        s_p.write_all(&p_pkt).await.unwrap();
        s_p.finish().unwrap();

        let idr_pkt = encode_video_packet(
            &VideoFrameHeader {
                sender_id: id_a,
                frame_seq: 2,
                timestamp_us: 2000,
                width: 1920,
                height: 1080,
                flags: VIDEO_FLAG_KEYFRAME | VIDEO_FLAG_H264,
            },
            b"idr-frame",
        )
        .unwrap();
        let mut s_idr = conn_a.open_uni().await.unwrap();
        s_idr.write_all(&idr_pkt).await.unwrap();
        s_idr.finish().unwrap();

        let a_pkt = encode_audio_packet(
            &AudioPacketHeader {
                sender_id: id_a,
                audio_seq: 1,
                timestamp_ms: 20,
                flags: AUDIO_FLAG_OPUS,
            },
            b"opus-20ms",
        )
        .unwrap();
        let _ = conn_a.send_datagram(a_pkt);

        // Bob receives the IDR frame (seq=2), NOT the orphan P-frame (seq=1)
        let mut uni_b = tokio::time::timeout(Duration::from_secs(3), conn_b.accept_uni())
            .await
            .expect("Bob receive video stream")
            .unwrap();
        let got_vid = uni_b.read_to_end(4096).await.unwrap();
        let (hdr, payload) = VideoFrameHeader::decode(&got_vid).unwrap();
        assert_eq!(hdr.frame_seq, 2);
        assert_eq!(payload, b"idr-frame");

        // Bob receives audio datagram
        let got_aud = tokio::time::timeout(Duration::from_secs(3), conn_b.read_datagram())
            .await
            .expect("Bob receive audio datagram")
            .unwrap();
        let (ahdr, apayload) = AudioPacketHeader::decode(&got_aud).unwrap();
        assert_eq!(ahdr.sender_id, id_a);
        assert_eq!(apayload, b"opus-20ms");

        // Carol (unsubscribed) receives ZERO video or audio
        assert!(
            tokio::time::timeout(Duration::from_millis(150), conn_c.accept_uni())
                .await
                .is_err()
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(50), conn_c.read_datagram())
                .await
                .is_err()
        );

        // 5. Cleanup checks
        drop(send_a);
        drop(recv_a);
        conn_a.close(0u32.into(), b"bye");
        ep_a.close(0u32.into(), b"bye");

        assert!(matches!(
            read_srv(&mut recv_b).await,
            ServerControl::PeerLeft { peer_id } if peer_id == id_a
        ));
        assert_ne!(id_b, id_a);

        drop(send_b);
        drop(recv_b);
        conn_b.close(0u32.into(), b"bye");
        ep_b.close(0u32.into(), b"bye");

        drop(send_c);
        drop(recv_c);
        conn_c.close(0u32.into(), b"bye");
        ep_c.close(0u32.into(), b"bye");

        for _ in 0..50 {
            if state.active_room_count() == 0 && state.active_peer_count() == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(state.active_room_count(), 0);
        assert_eq!(state.active_peer_count(), 0);

        let _: Bytes = idr_pkt;
        handle.abort();
    }
}
