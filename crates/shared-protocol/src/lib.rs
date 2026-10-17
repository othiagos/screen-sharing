use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

pub type RoomId = String;
pub type PeerId = Uuid;

/// Length of the binary video frame header (`[u32 width, u32 height, [u8; 16] peer_uuid]`).
pub const FRAME_HEADER_LEN: usize = 24;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ProtocolError {
    #[error("Frame buffer too short: expected at least {expected} bytes, got {actual}")]
    FrameBufferTooShort { expected: usize, actual: usize },
    #[error("Frame payload size mismatch for {width}x{height}: expected {expected} RGBA bytes, got {actual}")]
    FramePayloadSizeMismatch {
        width: u32,
        height: u32,
        expected: usize,
        actual: usize,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IceServerConfig {
    pub urls: Vec<String>,
    pub username: Option<String>,
    pub credential: Option<String>,
}

impl IceServerConfig {
    pub fn default_stun_servers() -> Vec<Self> {
        vec![
            Self {
                urls: vec!["stun:stun.l.google.com:19302".to_string()],
                username: None,
                credential: None,
            },
            Self {
                urls: vec!["stun:stun.cloudflare.com:3478".to_string()],
                username: None,
                credential: None,
            },
        ]
    }
}

/// Messages sent from a client to the WebSocket `signaling-server`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", content = "payload")]
pub enum ClientSignal {
    CreateRoom,
    JoinRoom {
        room_id: RoomId,
    },
    LeaveRoom,
    SdpOffer {
        target: PeerId,
        sdp: String,
    },
    SdpAnswer {
        target: PeerId,
        sdp: String,
    },
    IceCandidate {
        target: PeerId,
        candidate: String,
        sdp_mid: Option<String>,
        sdp_mline_index: Option<u16>,
    },
}

/// Messages sent from the WebSocket `signaling-server` to a client.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", content = "payload")]
pub enum ServerSignal {
    RoomCreated {
        room_id: RoomId,
        local_peer_id: PeerId,
    },
    RoomJoined {
        room_id: RoomId,
        local_peer_id: PeerId,
        existing_peers: Vec<PeerId>,
    },
    PeerJoined {
        peer_id: PeerId,
    },
    PeerLeft {
        peer_id: PeerId,
    },
    IncomingOffer {
        from: PeerId,
        sdp: String,
    },
    IncomingAnswer {
        from: PeerId,
        sdp: String,
    },
    IncomingIceCandidate {
        from: PeerId,
        candidate: String,
        sdp_mid: Option<String>,
        sdp_mline_index: Option<u16>,
    },
    Error {
        message: String,
    },
}

/// Messages exchanged directly P2P over the `"mesh-control"` RTCDataChannel
/// for serverless renegotiation and stream lifecycle events.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", content = "payload")]
pub enum PeerControlMessage {
    RenegotiateOffer {
        sdp: String,
    },
    RenegotiateAnswer {
        sdp: String,
    },
    IceCandidate {
        candidate: String,
        sdp_mid: Option<String>,
        sdp_mline_index: Option<u16>,
    },
    StreamStopped,
    RequestKeyframe,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum CaptureTarget {
    Monitor { id: u32, name: String },
    Window { id: u32, title: String, pid: u32 },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum TargetResolution {
    R720p,  // Max 1280x720 (preserve aspect ratio, even dimensions)
    R1080p, // Max 1920x1080 (preserve aspect ratio, even dimensions)
    Native, // Source resolution (clamped to codec limits & even dimensions)
}

impl TargetResolution {
    /// Computes the aspect-ratio preserving even target `(width, height)` for a given source size.
    /// When `clamp_1080p_for_native` is true, `Native` is also bounded within `1920x1080` so
    /// baseline H.264 encoders (like Cisco OpenH264) never exceed their macroblock ceiling.
    pub fn compute_dimensions(
        self,
        src_width: u32,
        src_height: u32,
        clamp_1080p_for_native: bool,
    ) -> (u32, u32) {
        let src_w = src_width.max(2);
        let src_h = src_height.max(2);

        let (max_w, max_h) = match self {
            TargetResolution::R720p => (1280u32, 720u32),
            TargetResolution::R1080p => (1920u32, 1080u32),
            TargetResolution::Native => {
                if clamp_1080p_for_native && (src_w > 1920 || src_h > 1080) {
                    (1920u32, 1080u32)
                } else {
                    return ((src_w & !1).max(2), (src_h & !1).max(2));
                }
            }
        };

        if src_w <= max_w && src_h <= max_h {
            return ((src_w & !1).max(2), (src_h & !1).max(2));
        }

        let scale_w = max_w as f64 / src_w as f64;
        let scale_h = max_h as f64 / src_h as f64;
        let scale = scale_w.min(scale_h);

        let scaled_w = ((src_w as f64 * scale).round() as u32 & !1).max(2);
        let scaled_h = ((src_h as f64 * scale).round() as u32 & !1).max(2);
        (scaled_w, scaled_h)
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum AudioCaptureMode {
    None,
    SystemGlobal,
    ApplicationOnly { pid: u32 },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StreamConfig {
    pub target: CaptureTarget,
    pub resolution: TargetResolution,
    pub fps: u8, // 15, 30, or 60
    pub audio_mode: AudioCaptureMode,
}

impl StreamConfig {
    pub fn sanitized_fps(&self) -> u8 {
        match self.fps {
            0..=22 => 15,
            23..=45 => 30,
            _ => 60,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FramePixelFormat {
    Rgba,
    I420,
}

/// Encodes a binary RGBA video frame with a 24-byte header:
/// `[u32 width (LE), u32 height (LE), [u8; 16] peer_uuid, ...rgba_bytes]`
pub fn encode_frame_packet(
    peer_id: PeerId,
    width: u32,
    height: u32,
    rgba: &[u8],
) -> Result<Vec<u8>, ProtocolError> {
    let expected = (width as usize)
        .checked_mul(height as usize)
        .and_then(|px| px.checked_mul(4))
        .unwrap_or(0);
    if rgba.len() != expected {
        return Err(ProtocolError::FramePayloadSizeMismatch {
            width,
            height,
            expected,
            actual: rgba.len(),
        });
    }

    let mut buf = Vec::with_capacity(FRAME_HEADER_LEN + rgba.len());
    buf.extend_from_slice(&width.to_le_bytes());
    buf.extend_from_slice(&height.to_le_bytes());
    buf.extend_from_slice(peer_id.as_bytes());
    buf.extend_from_slice(rgba);
    Ok(buf)
}

/// Encodes a binary planar I420 (`YUV420p`, 1.5 bytes/pixel) video frame with a 24-byte header:
/// `[u32 width (LE), u32 height (LE), [u8; 16] peer_uuid, ...y_plane, ...u_plane, ...v_plane]`
pub fn encode_yuv420p_frame_packet(
    peer_id: PeerId,
    width: u32,
    height: u32,
    i420: &[u8],
) -> Result<Vec<u8>, ProtocolError> {
    let y_len = (width as usize).checked_mul(height as usize).unwrap_or(0);
    let expected = y_len + (y_len / 2);
    if i420.len() != expected {
        return Err(ProtocolError::FramePayloadSizeMismatch {
            width,
            height,
            expected,
            actual: i420.len(),
        });
    }

    let mut buf = Vec::with_capacity(FRAME_HEADER_LEN + i420.len());
    buf.extend_from_slice(&width.to_le_bytes());
    buf.extend_from_slice(&height.to_le_bytes());
    buf.extend_from_slice(peer_id.as_bytes());
    buf.extend_from_slice(i420);
    Ok(buf)
}

/// Decodes a 24-byte header binary video frame into `(PeerId, width, height, rgba_slice)`.
pub fn decode_frame_packet(packet: &[u8]) -> Result<(PeerId, u32, u32, &[u8]), ProtocolError> {
    if packet.len() < FRAME_HEADER_LEN {
        return Err(ProtocolError::FrameBufferTooShort {
            expected: FRAME_HEADER_LEN,
            actual: packet.len(),
        });
    }

    let width = u32::from_le_bytes([packet[0], packet[1], packet[2], packet[3]]);
    let height = u32::from_le_bytes([packet[4], packet[5], packet[6], packet[7]]);
    let mut uuid_bytes = [0u8; 16];
    uuid_bytes.copy_from_slice(&packet[8..24]);
    let peer_id = Uuid::from_bytes(uuid_bytes);

    let rgba = &packet[FRAME_HEADER_LEN..];
    let expected = (width as usize)
        .checked_mul(height as usize)
        .and_then(|px| px.checked_mul(4))
        .unwrap_or(0);
    if rgba.len() != expected {
        return Err(ProtocolError::FramePayloadSizeMismatch {
            width,
            height,
            expected,
            actual: rgba.len(),
        });
    }

    Ok((peer_id, width, height, rgba))
}

/// Decodes either an RGBA (`w * h * 4`) or planar I420 (`w * h * 3 / 2`) binary video frame packet.
pub fn decode_any_frame_packet(
    packet: &[u8],
) -> Result<(PeerId, u32, u32, FramePixelFormat, &[u8]), ProtocolError> {
    if packet.len() < FRAME_HEADER_LEN {
        return Err(ProtocolError::FrameBufferTooShort {
            expected: FRAME_HEADER_LEN,
            actual: packet.len(),
        });
    }

    let width = u32::from_le_bytes([packet[0], packet[1], packet[2], packet[3]]);
    let height = u32::from_le_bytes([packet[4], packet[5], packet[6], packet[7]]);
    let mut uuid_bytes = [0u8; 16];
    uuid_bytes.copy_from_slice(&packet[8..24]);
    let peer_id = Uuid::from_bytes(uuid_bytes);

    let payload = &packet[FRAME_HEADER_LEN..];
    let px = (width as usize).checked_mul(height as usize).unwrap_or(0);
    let expected_rgba = px.saturating_mul(4);
    let expected_i420 = px + (px / 2);

    if payload.len() == expected_i420 && expected_i420 > 0 {
        Ok((peer_id, width, height, FramePixelFormat::I420, payload))
    } else if payload.len() == expected_rgba && expected_rgba > 0 {
        Ok((peer_id, width, height, FramePixelFormat::Rgba, payload))
    } else {
        Err(ProtocolError::FramePayloadSizeMismatch {
            width,
            height,
            expected: expected_i420,
            actual: payload.len(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_client_signal_serialization_roundtrip() {
        let peer_id = Uuid::new_v4();
        let signals = vec![
            ClientSignal::CreateRoom,
            ClientSignal::JoinRoom {
                room_id: "room-alpha-42".to_string(),
            },
            ClientSignal::LeaveRoom,
            ClientSignal::SdpOffer {
                target: peer_id,
                sdp: "v=0\r\no=- 123 2 IN IP4 127.0.0.1\r\n".to_string(),
            },
            ClientSignal::SdpAnswer {
                target: peer_id,
                sdp: "v=0\r\no=- 456 2 IN IP4 127.0.0.1\r\n".to_string(),
            },
            ClientSignal::IceCandidate {
                target: peer_id,
                candidate: "candidate:1 1 UDP 2130706431 192.168.1.10 50000 typ host".to_string(),
                sdp_mid: Some("0".to_string()),
                sdp_mline_index: Some(0),
            },
        ];

        for signal in signals {
            let json = serde_json::to_string(&signal).expect("serialize ClientSignal");
            let decoded: ClientSignal =
                serde_json::from_str(&json).expect("deserialize ClientSignal");
            assert_eq!(signal, decoded);
        }
    }

    #[test]
    fn test_server_signal_serialization_roundtrip() {
        let peer_a = Uuid::new_v4();
        let peer_b = Uuid::new_v4();
        let signals = vec![
            ServerSignal::RoomCreated {
                room_id: "room-1".to_string(),
                local_peer_id: peer_a,
            },
            ServerSignal::RoomJoined {
                room_id: "room-1".to_string(),
                local_peer_id: peer_b,
                existing_peers: vec![peer_a],
            },
            ServerSignal::PeerJoined { peer_id: peer_b },
            ServerSignal::PeerLeft { peer_id: peer_b },
            ServerSignal::IncomingOffer {
                from: peer_a,
                sdp: "offer-sdp".to_string(),
            },
            ServerSignal::IncomingAnswer {
                from: peer_b,
                sdp: "answer-sdp".to_string(),
            },
            ServerSignal::IncomingIceCandidate {
                from: peer_a,
                candidate: "candidate:2".to_string(),
                sdp_mid: None,
                sdp_mline_index: Some(1),
            },
            ServerSignal::Error {
                message: "Room not found".to_string(),
            },
        ];

        for signal in signals {
            let json = serde_json::to_string(&signal).expect("serialize ServerSignal");
            let decoded: ServerSignal =
                serde_json::from_str(&json).expect("deserialize ServerSignal");
            assert_eq!(signal, decoded);
        }
    }

    #[test]
    fn test_peer_control_and_stream_config_roundtrip() {
        let control_msgs = vec![
            PeerControlMessage::RenegotiateOffer {
                sdp: "v=0...".to_string(),
            },
            PeerControlMessage::RenegotiateAnswer {
                sdp: "v=0...".to_string(),
            },
            PeerControlMessage::IceCandidate {
                candidate: "candidate:1".to_string(),
                sdp_mid: Some("video".to_string()),
                sdp_mline_index: Some(0),
            },
            PeerControlMessage::StreamStopped,
            PeerControlMessage::RequestKeyframe,
        ];

        for msg in control_msgs {
            let json = serde_json::to_string(&msg).expect("serialize PeerControlMessage");
            let decoded: PeerControlMessage =
                serde_json::from_str(&json).expect("deserialize PeerControlMessage");
            assert_eq!(msg, decoded);
        }

        let cfg = StreamConfig {
            target: CaptureTarget::Window {
                id: 101,
                title: "Firefox".to_string(),
                pid: 4242,
            },
            resolution: TargetResolution::R1080p,
            fps: 60,
            audio_mode: AudioCaptureMode::ApplicationOnly { pid: 4242 },
        };

        let json = serde_json::to_string(&cfg).expect("serialize StreamConfig");
        let decoded: StreamConfig = serde_json::from_str(&json).expect("deserialize StreamConfig");
        assert_eq!(cfg, decoded);
    }

    #[test]
    fn test_target_resolution_even_dimensions_and_aspect_ratio() {
        // Odd window size 1365x767 scaled to 720p must yield even dimensions
        let (w, h) = TargetResolution::R720p.compute_dimensions(1365, 767, true);
        assert_eq!(w % 2, 0);
        assert_eq!(h % 2, 0);
        assert!(w <= 1280 && h <= 720);

        // Native with odd dimensions must round down to even
        let (nw, nh) = TargetResolution::Native.compute_dimensions(1023, 767, true);
        assert_eq!((nw, nh), (1022, 766));

        // 4K scaled to 1080p
        let (fw, fh) = TargetResolution::R1080p.compute_dimensions(3840, 2160, true);
        assert_eq!((fw, fh), (1920, 1080));
    }

    #[test]
    fn test_binary_frame_packet_encode_decode() {
        let peer_id = Uuid::new_v4();
        let width = 4u32;
        let height = 2u32;
        let rgba = vec![128u8; (width * height * 4) as usize];

        let packet = encode_frame_packet(peer_id, width, height, &rgba).expect("encode packet");
        assert_eq!(packet.len(), FRAME_HEADER_LEN + rgba.len());

        let (dec_peer, dec_w, dec_h, dec_rgba) =
            decode_frame_packet(&packet).expect("decode packet");
        assert_eq!(dec_peer, peer_id);
        assert_eq!(dec_w, width);
        assert_eq!(dec_h, height);
        assert_eq!(dec_rgba, rgba.as_slice());

        // I420 (YUV420p) packet roundtrip
        let i420 = vec![64u8; ((width * height * 3) / 2) as usize];
        let yuv_packet =
            encode_yuv420p_frame_packet(peer_id, width, height, &i420).expect("encode i420 packet");
        assert_eq!(yuv_packet.len(), FRAME_HEADER_LEN + i420.len());

        let (dec_yuv_peer, dec_yuv_w, dec_yuv_h, fmt, dec_i420) =
            decode_any_frame_packet(&yuv_packet).expect("decode i420 packet");
        assert_eq!(dec_yuv_peer, peer_id);
        assert_eq!((dec_yuv_w, dec_yuv_h), (width, height));
        assert_eq!(fmt, FramePixelFormat::I420);
        assert_eq!(dec_i420, i420.as_slice());

        // Short packet error
        assert!(matches!(
            decode_frame_packet(&[0u8; 10]),
            Err(ProtocolError::FrameBufferTooShort { .. })
        ));
    }
}
