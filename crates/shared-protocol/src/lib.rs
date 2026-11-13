use bytes::{BufMut, Bytes, BytesMut};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use uuid::Uuid;

pub type RoomId = String;
pub type PeerId = Uuid;

pub const PROTOCOL_VERSION: u16 = 2;
pub const ALPN_SFU_QUIC: &[u8] = b"screenshare-sfu/1";

pub const MAX_CONTROL_FRAME_BYTES: usize = 64 * 1024;
pub const MAX_VIDEO_FRAME_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_AUDIO_PACKET_BYTES: usize = 4 * 1024;

pub const MEDIA_KIND_VIDEO: u8 = 1;
pub const MEDIA_KIND_AUDIO: u8 = 2;

pub const VIDEO_FLAG_KEYFRAME: u8 = 0x01;
pub const VIDEO_FLAG_H264: u8 = 0x02;
pub const AUDIO_FLAG_OPUS: u8 = 0x01;

/// Fixed 36-byte header for QUIC video frame UniStreams:
/// `[kind(1), flags(1), reserved(2), frame_seq(4), timestamp_us(8), width(2), height(2), sender_uuid(16)]`
pub const VIDEO_HEADER_LEN: usize = 36;

/// Fixed 28-byte header for QUIC audio packets (Datagram or fallback UniStream):
/// `[kind(1), flags(1), reserved(2), audio_seq(4), timestamp_ms(4), sender_uuid(16)]`
pub const AUDIO_HEADER_LEN: usize = 28;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ProtocolError {
    #[error("Control frame too short: expected at least {expected} bytes, got {actual}")]
    FrameTooShort { expected: usize, actual: usize },
    #[error("Payload exceeds maximum allowed size ({actual} > {max})")]
    PayloadTooLarge { actual: usize, max: usize },
    #[error("Unexpected media kind byte: expected {expected}, got {actual}")]
    UnexpectedMediaKind { expected: u8, actual: u8 },
    #[error("Control message serialization error: {0}")]
    Serialization(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum CaptureTarget {
    Monitor { id: u32, name: String },
    Window { id: u32, title: String, pid: u32 },
    PortalWayland,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
pub enum TargetResolution {
    R720p,
    #[default]
    R1080p,
    R1440p,
    Native,
}

impl TargetResolution {
    pub fn label(self) -> &'static str {
        match self {
            Self::R720p => "720p",
            Self::R1080p => "1080p",
            Self::R1440p => "1440p",
            Self::Native => "Native",
        }
    }

    /// Computes aspect-ratio-preserving even `(width, height)` for a given source size.
    /// When `clamp_1080p_for_sw` is true (software OpenH264 fallback ceiling), `Native` and `R1440p`
    /// can be bounded within `1920x1080` if needed; hardware encoders keep `Native` unclamped.
    pub fn compute_dimensions(
        self,
        src_width: u32,
        src_height: u32,
        clamp_1080p_for_sw: bool,
    ) -> (u32, u32) {
        let src_w = src_width.max(2);
        let src_h = src_height.max(2);

        let (max_w, max_h) = match self {
            Self::R720p => (1280u32, 720u32),
            Self::R1080p => (1920u32, 1080u32),
            Self::R1440p => {
                if clamp_1080p_for_sw {
                    (1920u32, 1080u32)
                } else {
                    (2560u32, 1440u32)
                }
            }
            Self::Native => {
                if clamp_1080p_for_sw && (src_w > 1920 || src_h > 1080) {
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

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
pub enum BitratePreset {
    Mbps6,
    #[default]
    Mbps12,
    Mbps20,
    Auto,
}

impl BitratePreset {
    pub fn label(self) -> &'static str {
        match self {
            Self::Mbps6 => "6 Mbps",
            Self::Mbps12 => "12 Mbps",
            Self::Mbps20 => "20 Mbps",
            Self::Auto => "Auto",
        }
    }

    pub fn initial_bps(self, resolution: TargetResolution, fps: u8) -> u32 {
        match self {
            Self::Mbps6 => 6_000_000,
            Self::Mbps12 => 12_000_000,
            Self::Mbps20 => 20_000_000,
            Self::Auto => {
                let base = match resolution {
                    TargetResolution::R720p => 6_000_000,
                    TargetResolution::R1080p => 12_000_000,
                    TargetResolution::R1440p | TargetResolution::Native => 16_000_000,
                };
                if fps <= 20 {
                    base / 2
                } else if fps <= 35 {
                    (base * 3) / 4
                } else {
                    base
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
pub enum AudioCaptureMode {
    None,
    #[default]
    SystemGlobal,
    ApplicationOnly { pid: u32 },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StreamConfig {
    pub target: CaptureTarget,
    pub resolution: TargetResolution,
    pub fps: u8,
    pub bitrate: BitratePreset,
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StreamMetadata {
    pub width: u32,
    pub height: u32,
    pub fps: u8,
    pub has_audio: bool,
    pub codec_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PeerInfo {
    pub peer_id: PeerId,
    pub nickname: String,
    pub active_stream: Option<StreamMetadata>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum ControlErrorCode {
    ProtocolMismatch,
    Unauthorized,
    InvalidPassword,
    RoomNotFound,
    NotInRoom,
    BadRequest,
    Internal,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", content = "payload")]
pub enum ClientControl {
    Hello {
        protocol_version: u16,
        nickname: String,
        auth_token: Option<String>,
    },
    CreateRoom {
        password: Option<String>,
    },
    JoinRoom {
        room_id: RoomId,
        password: Option<String>,
    },
    LeaveRoom,
    StartBroadcast {
        metadata: StreamMetadata,
    },
    StopBroadcast,
    SetSubscription {
        broadcaster_id: PeerId,
        video: bool,
        audio: bool,
    },
    RequestKeyframe {
        broadcaster_id: PeerId,
    },
    Ping {
        client_time_ms: u64,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", content = "payload")]
pub enum ServerControl {
    Welcome {
        local_peer_id: PeerId,
    },
    RoomJoined {
        room_id: RoomId,
        local_peer_id: PeerId,
        peers: Vec<PeerInfo>,
    },
    PeerJoined {
        peer: PeerInfo,
    },
    PeerLeft {
        peer_id: PeerId,
    },
    PeerStreamStarted {
        peer_id: PeerId,
        metadata: StreamMetadata,
    },
    PeerStreamStopped {
        peer_id: PeerId,
    },
    ForceKeyframe,
    Pong {
        client_time_ms: u64,
    },
    Error {
        code: ControlErrorCode,
        message: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoFrameHeader {
    pub sender_id: PeerId,
    pub frame_seq: u32,
    pub timestamp_us: u64,
    pub width: u16,
    pub height: u16,
    pub flags: u8,
}

impl VideoFrameHeader {
    pub fn is_keyframe(&self) -> bool {
        (self.flags & VIDEO_FLAG_KEYFRAME) != 0
    }

    pub fn encode_into(&self, dst: &mut BytesMut) {
        dst.put_u8(MEDIA_KIND_VIDEO);
        dst.put_u8(self.flags);
        dst.put_u16_le(0);
        dst.put_u32_le(self.frame_seq);
        dst.put_u64_le(self.timestamp_us);
        dst.put_u16_le(self.width);
        dst.put_u16_le(self.height);
        dst.put_slice(self.sender_id.as_bytes());
    }

    pub fn decode(buf: &[u8]) -> Result<(Self, &[u8]), ProtocolError> {
        if buf.len() < VIDEO_HEADER_LEN {
            return Err(ProtocolError::FrameTooShort {
                expected: VIDEO_HEADER_LEN,
                actual: buf.len(),
            });
        }
        if buf[0] != MEDIA_KIND_VIDEO {
            return Err(ProtocolError::UnexpectedMediaKind {
                expected: MEDIA_KIND_VIDEO,
                actual: buf[0],
            });
        }
        let flags = buf[1];
        let frame_seq = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);
        let timestamp_us = u64::from_le_bytes([
            buf[8], buf[9], buf[10], buf[11], buf[12], buf[13], buf[14], buf[15],
        ]);
        let width = u16::from_le_bytes([buf[16], buf[17]]);
        let height = u16::from_le_bytes([buf[18], buf[19]]);
        let mut uuid_bytes = [0u8; 16];
        uuid_bytes.copy_from_slice(&buf[20..36]);

        Ok((
            Self {
                sender_id: Uuid::from_bytes(uuid_bytes),
                frame_seq,
                timestamp_us,
                width,
                height,
                flags,
            },
            &buf[VIDEO_HEADER_LEN..],
        ))
    }
}

pub fn encode_video_packet(
    header: &VideoFrameHeader,
    payload: &[u8],
) -> Result<Bytes, ProtocolError> {
    if payload.len() > MAX_VIDEO_FRAME_BYTES {
        return Err(ProtocolError::PayloadTooLarge {
            actual: payload.len(),
            max: MAX_VIDEO_FRAME_BYTES,
        });
    }
    let mut buf = BytesMut::with_capacity(VIDEO_HEADER_LEN + payload.len());
    header.encode_into(&mut buf);
    buf.put_slice(payload);
    Ok(buf.freeze())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioPacketHeader {
    pub sender_id: PeerId,
    pub audio_seq: u32,
    pub timestamp_ms: u32,
    pub flags: u8,
}

impl AudioPacketHeader {
    pub fn encode_into(&self, dst: &mut BytesMut) {
        dst.put_u8(MEDIA_KIND_AUDIO);
        dst.put_u8(self.flags);
        dst.put_u16_le(0);
        dst.put_u32_le(self.audio_seq);
        dst.put_u32_le(self.timestamp_ms);
        dst.put_slice(self.sender_id.as_bytes());
    }

    pub fn decode(buf: &[u8]) -> Result<(Self, &[u8]), ProtocolError> {
        if buf.len() < AUDIO_HEADER_LEN {
            return Err(ProtocolError::FrameTooShort {
                expected: AUDIO_HEADER_LEN,
                actual: buf.len(),
            });
        }
        if buf[0] != MEDIA_KIND_AUDIO {
            return Err(ProtocolError::UnexpectedMediaKind {
                expected: MEDIA_KIND_AUDIO,
                actual: buf[0],
            });
        }
        let flags = buf[1];
        let audio_seq = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);
        let timestamp_ms = u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]);
        let mut uuid_bytes = [0u8; 16];
        uuid_bytes.copy_from_slice(&buf[12..28]);

        Ok((
            Self {
                sender_id: Uuid::from_bytes(uuid_bytes),
                audio_seq,
                timestamp_ms,
                flags,
            },
            &buf[AUDIO_HEADER_LEN..],
        ))
    }
}

pub fn encode_audio_packet(
    header: &AudioPacketHeader,
    opus_payload: &[u8],
) -> Result<Bytes, ProtocolError> {
    if opus_payload.len() > MAX_AUDIO_PACKET_BYTES {
        return Err(ProtocolError::PayloadTooLarge {
            actual: opus_payload.len(),
            max: MAX_AUDIO_PACKET_BYTES,
        });
    }
    let mut buf = BytesMut::with_capacity(AUDIO_HEADER_LEN + opus_payload.len());
    header.encode_into(&mut buf);
    buf.put_slice(opus_payload);
    Ok(buf.freeze())
}

pub fn encode_control_frame<T: Serialize>(msg: &T) -> Result<Bytes, ProtocolError> {
    let json = serde_json::to_vec(msg).map_err(|e| ProtocolError::Serialization(e.to_string()))?;
    if json.len() > MAX_CONTROL_FRAME_BYTES {
        return Err(ProtocolError::PayloadTooLarge {
            actual: json.len(),
            max: MAX_CONTROL_FRAME_BYTES,
        });
    }
    let mut buf = BytesMut::with_capacity(4 + json.len());
    buf.put_u32_le(json.len() as u32);
    buf.put_slice(&json);
    Ok(buf.freeze())
}

pub fn decode_control_payload<T: DeserializeOwned>(payload: &[u8]) -> Result<T, ProtocolError> {
    if payload.len() > MAX_CONTROL_FRAME_BYTES {
        return Err(ProtocolError::PayloadTooLarge {
            actual: payload.len(),
            max: MAX_CONTROL_FRAME_BYTES,
        });
    }
    serde_json::from_slice(payload).map_err(|e| ProtocolError::Serialization(e.to_string()))
}

pub fn format_cert_sha256_fingerprint(der: &[u8]) -> String {
    let digest = Sha256::digest(der);
    let mut out = String::with_capacity(digest.len() * 3 - 1);
    for (i, byte) in digest.iter().enumerate() {
        if i > 0 {
            out.push(':');
        }
        use std::fmt::Write;
        let _ = write!(&mut out, "{byte:02X}");
    }
    out
}

pub fn normalize_fingerprint(input: &str) -> String {
    input
        .chars()
        .filter(|c| c.is_ascii_hexdigit())
        .map(|c| c.to_ascii_uppercase())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_control_roundtrip() {
        let peer_id = Uuid::new_v4();
        let client_msgs = vec![
            ClientControl::Hello {
                protocol_version: PROTOCOL_VERSION,
                nickname: "Player1".to_string(),
                auth_token: Some("secret".to_string()),
            },
            ClientControl::CreateRoom {
                password: Some("1234".to_string()),
            },
            ClientControl::JoinRoom {
                room_id: "ab12cd34".to_string(),
                password: None,
            },
            ClientControl::StartBroadcast {
                metadata: StreamMetadata {
                    width: 2560,
                    height: 1440,
                    fps: 60,
                    has_audio: true,
                    codec_name: "H.264".to_string(),
                },
            },
            ClientControl::SetSubscription {
                broadcaster_id: peer_id,
                video: true,
                audio: true,
            },
            ClientControl::Ping {
                client_time_ms: 4242,
            },
        ];

        for msg in client_msgs {
            let framed = encode_control_frame(&msg).expect("encode ClientControl");
            let len = u32::from_le_bytes([framed[0], framed[1], framed[2], framed[3]]) as usize;
            assert_eq!(len, framed.len() - 4);
            let decoded: ClientControl =
                decode_control_payload(&framed[4..]).expect("decode ClientControl");
            assert_eq!(msg, decoded);
        }

        let server_msg = ServerControl::RoomJoined {
            room_id: "ab12cd34".to_string(),
            local_peer_id: peer_id,
            peers: vec![PeerInfo {
                peer_id,
                nickname: "Player1".to_string(),
                active_stream: None,
            }],
        };
        let framed = encode_control_frame(&server_msg).expect("encode ServerControl");
        let decoded: ServerControl =
            decode_control_payload(&framed[4..]).expect("decode ServerControl");
        assert_eq!(server_msg, decoded);
    }

    #[test]
    fn test_video_and_audio_binary_packets() {
        let sender_id = Uuid::new_v4();
        let v_hdr = VideoFrameHeader {
            sender_id,
            frame_seq: 99,
            timestamp_us: 123_456_789,
            width: 2560,
            height: 1440,
            flags: VIDEO_FLAG_KEYFRAME | VIDEO_FLAG_H264,
        };
        let payload = vec![0x00, 0x00, 0x00, 0x01, 0x65, 0xAA];
        let v_pkt = encode_video_packet(&v_hdr, &payload).expect("encode video packet");
        assert_eq!(v_pkt.len(), VIDEO_HEADER_LEN + payload.len());
        let (dec_hdr, dec_payload) = VideoFrameHeader::decode(&v_pkt).expect("decode video");
        assert_eq!(dec_hdr, v_hdr);
        assert!(dec_hdr.is_keyframe());
        assert_eq!(dec_payload, payload.as_slice());

        let a_hdr = AudioPacketHeader {
            sender_id,
            audio_seq: 777,
            timestamp_ms: 42_000,
            flags: AUDIO_FLAG_OPUS,
        };
        let opus = vec![0xFC, 0x10, 0x20];
        let a_pkt = encode_audio_packet(&a_hdr, &opus).expect("encode audio packet");
        assert_eq!(a_pkt.len(), AUDIO_HEADER_LEN + opus.len());
        let (dec_a_hdr, dec_opus) = AudioPacketHeader::decode(&a_pkt).expect("decode audio");
        assert_eq!(dec_a_hdr, a_hdr);
        assert_eq!(dec_opus, opus.as_slice());
    }

    #[test]
    fn test_target_resolution_1440p_and_native_unclamped() {
        assert_eq!(
            TargetResolution::R1440p.compute_dimensions(3840, 2160, false),
            (2560, 1440)
        );
        assert_eq!(
            TargetResolution::Native.compute_dimensions(2559, 1439, false),
            (2558, 1438)
        );
        assert_eq!(
            TargetResolution::Native.compute_dimensions(2560, 1440, true),
            (1920, 1080)
        );
    }

    #[test]
    fn test_fingerprint_formatting_and_normalization() {
        let fp = format_cert_sha256_fingerprint(b"test-cert-der");
        assert_eq!(fp.split(':').count(), 32);
        let norm = normalize_fingerprint(&fp.to_lowercase());
        assert_eq!(norm.len(), 64);
        assert!(!norm.contains(':'));
    }
}
