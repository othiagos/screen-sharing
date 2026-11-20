use std::sync::Arc;
use std::time::{Duration, Instant};

use desktop_client::media::mock::{MockAudioCapturer, MockVideoCapturer};
use desktop_client::media::{
    AudioCapturer, LocalBroadcastSession, MediaError, PeerAudioMixer, PeerMediaReceiver,
    PeerVideoStore, RawAudioFrame, RawVideoFrame, VideoCapturer,
};
use desktop_client::net::{
    connect_sfu, ConnectOutcome, ConnectRequest, MediaSink, SfuClient, SfuEvent,
};
use sfu_server::tls::TlsSource;
use sfu_server::{spawn_sfu_server, SfuServerConfig};
use shared_protocol::{
    AudioCaptureMode, BitratePreset, CaptureTarget, ControlErrorCode, PeerId, StreamConfig,
    TargetResolution,
};
use tokio::sync::mpsc;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;

struct PeerHarness {
    client: SfuClient,
    events: mpsc::UnboundedReceiver<SfuEvent>,
    video_store: Arc<PeerVideoStore>,
    audio_mixer: PeerAudioMixer,
    receiver: Arc<PeerMediaReceiver>,
    cancel: CancellationToken,
}

async fn connect_peer(
    server_addr: &str,
    nickname: &str,
    auth_token: Option<&str>,
    fingerprint: &str,
) -> anyhow::Result<PeerHarness> {
    let cancel = CancellationToken::new();
    let video_store = PeerVideoStore::new();
    let audio_mixer = PeerAudioMixer::new(cancel.clone());
    let receiver = PeerMediaReceiver::new(Arc::clone(&video_store), audio_mixer.clone());
    let (ev_tx, ev_rx) = mpsc::unbounded_channel();

    let media_sink: Arc<dyn MediaSink> = receiver.clone();
    let outcome = connect_sfu(
        ConnectRequest {
            server_addr: server_addr.to_string(),
            nickname: nickname.to_string(),
            auth_token: auth_token.map(str::to_string),
            pinned_fingerprint: Some(fingerprint.to_string()),
        },
        ev_tx,
        media_sink,
    )
    .await?;

    let client = match outcome {
        ConnectOutcome::Connected(c) => c,
        ConnectOutcome::TrustRequired { .. } => {
            anyhow::bail!("Unexpected TrustRequired with pinned fingerprint");
        }
    };
    receiver.attach_sfu(Some(client.clone()));

    Ok(PeerHarness {
        client,
        events: ev_rx,
        video_store,
        audio_mixer,
        receiver,
        cancel,
    })
}

async fn wait_for_event<F, T>(
    rx: &mut mpsc::UnboundedReceiver<SfuEvent>,
    timeout: Duration,
    mut pred: F,
) -> anyhow::Result<T>
where
    F: FnMut(SfuEvent) -> Option<T>,
{
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            anyhow::bail!("Timed out waiting for expected SfuEvent");
        }
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Some(ev)) => {
                if let Some(matched) = pred(ev) {
                    return Ok(matched);
                }
            }
            Ok(None) => anyhow::bail!("Event channel closed unexpectedly"),
            Err(_) => anyhow::bail!("Timed out waiting for SfuEvent"),
        }
    }
}

#[tokio::test]
async fn test_headless_three_peer_quic_sfu_stream_and_auth() -> anyhow::Result<()> {
    let (bound_addr, fingerprint, _state, server_task) = spawn_sfu_server(SfuServerConfig {
        bind_addr: "127.0.0.1:0".parse()?,
        auth_token: Some("top-secret-token".to_string()),
        tls: TlsSource::SelfSigned {
            cert_dir: None,
            extra_sans: vec![],
        },
    })
    .await?;
    let addr_str = bound_addr.to_string();

    // 1. Verify wrong server auth token is rejected cleanly
    let bad_auth = connect_peer(&addr_str, "Intruder", Some("bad-token"), &fingerprint).await;
    if let Ok(mut intruder) = bad_auth {
        let rejected = wait_for_event(&mut intruder.events, Duration::from_secs(3), |ev| match ev {
            SfuEvent::ServerError {
                code: ControlErrorCode::Unauthorized,
                ..
            }
            | SfuEvent::Disconnected { .. } => Some(()),
            _ => None,
        })
        .await;
        assert!(rejected.is_ok(), "Expected Unauthorized or Disconnected");
        intruder.receiver.stop_all();
        intruder.cancel.cancel();
    }

    // 2. Broadcaster connects and creates password-protected room
    let mut broadcaster =
        connect_peer(&addr_str, "Broadcaster", Some("top-secret-token"), &fingerprint).await?;
    let broadcaster_id: PeerId = broadcaster.client.local_peer_id();
    broadcaster
        .client
        .create_room(Some("game-night".to_string()));
    let room_id = wait_for_event(&mut broadcaster.events, Duration::from_secs(3), |ev| {
        if let SfuEvent::RoomJoined { room_id, .. } = ev {
            Some(room_id)
        } else {
            None
        }
    })
    .await?;

    // 3. ViewerA tries wrong room password, then joins with correct password
    let mut viewer_a =
        connect_peer(&addr_str, "ViewerA", Some("top-secret-token"), &fingerprint).await?;
    viewer_a
        .client
        .join_room(room_id.clone(), Some("wrong-pass".to_string()));
    wait_for_event(&mut viewer_a.events, Duration::from_secs(3), |ev| match ev {
        SfuEvent::ServerError {
            code: ControlErrorCode::InvalidPassword,
            ..
        } => Some(()),
        _ => None,
    })
    .await?;

    viewer_a
        .client
        .join_room(room_id.clone(), Some("game-night".to_string()));
    wait_for_event(&mut viewer_a.events, Duration::from_secs(3), |ev| {
        if let SfuEvent::RoomJoined { .. } = ev {
            Some(())
        } else {
            None
        }
    })
    .await?;

    // 4. Broadcaster starts headless 640x360@30fps video + 440Hz audio stream
    let stream_cfg = StreamConfig {
        target: CaptureTarget::Monitor {
            id: 0,
            name: "MockMonitor".to_string(),
        },
        resolution: TargetResolution::R720p,
        fps: 30,
        bitrate: BitratePreset::Mbps6,
        audio_mode: AudioCaptureMode::SystemGlobal,
    };

    let broadcast = LocalBroadcastSession::start_with_capturers(
        broadcaster.client.clone(),
        stream_cfg,
        Arc::new(MockVideoCapturer::new(640, 360)),
        Arc::new(MockAudioCapturer::new()),
        Arc::clone(&broadcaster.video_store),
        None,
    )?;

    wait_for_event(&mut viewer_a.events, Duration::from_secs(3), |ev| match ev {
        SfuEvent::PeerBroadcastUpdated {
            peer_id,
            metadata: Some(_),
        } if peer_id == broadcaster_id => Some(()),
        _ => None,
    })
    .await?;

    // 5. ViewerA subscribes immediately and verifies >= 25 video frames + >= 35 audio packets
    viewer_a.client.set_subscription(broadcaster_id, true, true);
    viewer_a.client.request_keyframe(broadcaster_id);

    let deadline_a = Instant::now() + Duration::from_secs(6);
    let mut viewer_a_frames = 0u64;
    let mut viewer_a_audio_pkts = 0u64;
    while Instant::now() < deadline_a {
        viewer_a_frames = viewer_a.video_store.latest_peer_generation(broadcaster_id);
        viewer_a_audio_pkts = viewer_a
            .audio_mixer
            .peer_audio_stats(broadcaster_id)
            .map(|s| s.3)
            .unwrap_or(0);
        if viewer_a_frames >= 25 && viewer_a_audio_pkts >= 35 {
            break;
        }
        sleep(Duration::from_millis(30)).await;
    }

    assert!(
        viewer_a_frames >= 25,
        "ViewerA expected >= 25 decoded video frames, got {viewer_a_frames}"
    );
    assert!(
        viewer_a_audio_pkts >= 35,
        "ViewerA expected >= 35 decoded Opus audio packets, got {viewer_a_audio_pkts}"
    );
    let sample_frame = viewer_a
        .video_store
        .latest_peer_frame(broadcaster_id)
        .expect("ViewerA decoded frame missing");
    assert_eq!(sample_frame.width, 640);
    assert_eq!(sample_frame.height, 360);
    assert_eq!(
        sample_frame.bgra.len(),
        (sample_frame.width * sample_frame.height * 4) as usize
    );

    // 6. Late joiner ViewerB joins 700ms+ into stream, subscribes (verifies forced IDR keyframe)
    let mut viewer_b =
        connect_peer(&addr_str, "ViewerB", Some("top-secret-token"), &fingerprint).await?;
    viewer_b
        .client
        .join_room(room_id.clone(), Some("game-night".to_string()));
    wait_for_event(&mut viewer_b.events, Duration::from_secs(3), |ev| {
        if let SfuEvent::RoomJoined { .. } = ev {
            Some(())
        } else {
            None
        }
    })
    .await?;

    viewer_b.client.set_subscription(broadcaster_id, true, true);
    viewer_b.client.request_keyframe(broadcaster_id);

    let deadline_b = Instant::now() + Duration::from_secs(4);
    let mut viewer_b_frames = 0u64;
    while Instant::now() < deadline_b {
        viewer_b_frames = viewer_b.video_store.latest_peer_generation(broadcaster_id);
        if viewer_b_frames >= 4 {
            break;
        }
        sleep(Duration::from_millis(30)).await;
    }
    assert!(
        viewer_b_frames >= 4,
        "Late subscriber ViewerB expected >= 4 decoded frames via forced IDR, got {viewer_b_frames}"
    );

    // 7. ViewerB unsubscribes and stops receiving new frames while ViewerA keeps advancing
    viewer_b
        .client
        .set_subscription(broadcaster_id, false, false);
    sleep(Duration::from_millis(150)).await;
    let b_frozen_gen = viewer_b.video_store.latest_peer_generation(broadcaster_id);
    let a_before_gen = viewer_a.video_store.latest_peer_generation(broadcaster_id);

    sleep(Duration::from_millis(600)).await;
    let b_after_gen = viewer_b.video_store.latest_peer_generation(broadcaster_id);
    let a_after_gen = viewer_a.video_store.latest_peer_generation(broadcaster_id);

    assert_eq!(
        b_frozen_gen, b_after_gen,
        "Unsubscribed ViewerB should stop advancing video frames"
    );
    assert!(
        a_after_gen > a_before_gen,
        "ViewerA should continue advancing video frames ({a_before_gen} -> {a_after_gen})"
    );

    // Clean shutdown
    broadcast.stop(&broadcaster.client);
    wait_for_event(&mut viewer_a.events, Duration::from_secs(3), |ev| match ev {
        SfuEvent::PeerBroadcastUpdated {
            peer_id,
            metadata: None,
        } if peer_id == broadcaster_id => Some(()),
        _ => None,
    })
    .await?;

    broadcaster.receiver.stop_all();
    viewer_a.receiver.stop_all();
    viewer_b.receiver.stop_all();
    broadcaster.client.disconnect();
    viewer_a.client.disconnect();
    viewer_b.client.disconnect();
    broadcaster.cancel.cancel();
    viewer_a.cancel.cancel();
    viewer_b.cancel.cancel();
    server_task.abort();

    Ok(())
}

struct EndingVideoCapturer;

impl VideoCapturer for EndingVideoCapturer {
    fn start_capture(
        &self,
        _target: CaptureTarget,
        _resolution: TargetResolution,
        _fps: u8,
        tx: mpsc::Sender<RawVideoFrame>,
        _cancel: CancellationToken,
    ) -> Result<(), MediaError> {
        tokio::spawn(async move {
            for idx in 0..2u64 {
                let frame = RawVideoFrame {
                    width: 320,
                    height: 180,
                    rgba: MockVideoCapturer::generate_smpte_bars(320, 180, idx),
                    is_bgra: false,
                    timestamp_us: idx * 33_333,
                };
                if tx.send(frame).await.is_err() {
                    return;
                }
                sleep(Duration::from_millis(25)).await;
            }
            // Drop `tx` to simulate captured process/window termination.
        });
        Ok(())
    }
}

struct ObservingAudioCapturer {
    observed_cancel: Arc<parking_lot::Mutex<Option<CancellationToken>>>,
}

impl AudioCapturer for ObservingAudioCapturer {
    fn start_capture(
        &self,
        _mode: AudioCaptureMode,
        _tx: mpsc::Sender<RawAudioFrame>,
        cancel: CancellationToken,
    ) -> Result<(), MediaError> {
        *self.observed_cancel.lock() = Some(cancel);
        Ok(())
    }
}

#[tokio::test]
async fn test_unexpected_capture_end_stops_broadcast_and_notifies_peers() -> anyhow::Result<()> {
    let (bound_addr, fingerprint, _state, server_task) = spawn_sfu_server(SfuServerConfig {
        bind_addr: "127.0.0.1:0".parse()?,
        auth_token: None,
        tls: TlsSource::SelfSigned {
            cert_dir: None,
            extra_sans: vec![],
        },
    })
    .await?;
    let addr_str = bound_addr.to_string();

    let mut broadcaster = connect_peer(&addr_str, "Broadcaster", None, &fingerprint).await?;
    let broadcaster_id = broadcaster.client.local_peer_id();
    broadcaster.client.create_room(None);
    let room_id = wait_for_event(&mut broadcaster.events, Duration::from_secs(3), |ev| {
        if let SfuEvent::RoomJoined { room_id, .. } = ev {
            Some(room_id)
        } else {
            None
        }
    })
    .await?;

    let mut viewer = connect_peer(&addr_str, "Viewer", None, &fingerprint).await?;
    viewer.client.join_room(room_id, None);
    wait_for_event(&mut viewer.events, Duration::from_secs(3), |ev| {
        if let SfuEvent::RoomJoined { .. } = ev {
            Some(())
        } else {
            None
        }
    })
    .await?;

    let observed_audio_cancel = Arc::new(parking_lot::Mutex::new(None));
    let stream_cfg = StreamConfig {
        target: CaptureTarget::Window {
            id: 777,
            title: "ClosedGame".to_string(),
            pid: 99_999,
        },
        resolution: TargetResolution::R720p,
        fps: 30,
        bitrate: BitratePreset::Mbps6,
        audio_mode: AudioCaptureMode::SystemGlobal,
    };

    let broadcast = LocalBroadcastSession::start_with_capturers(
        broadcaster.client.clone(),
        stream_cfg,
        Arc::new(EndingVideoCapturer),
        Arc::new(ObservingAudioCapturer {
            observed_cancel: Arc::clone(&observed_audio_cancel),
        }),
        Arc::clone(&broadcaster.video_store),
        None,
    )?;

    wait_for_event(&mut viewer.events, Duration::from_secs(3), |ev| match ev {
        SfuEvent::PeerBroadcastUpdated {
            peer_id,
            metadata: Some(_),
        } if peer_id == broadcaster_id => Some(()),
        _ => None,
    })
    .await?;

    // Without calling broadcast.stop(), viewer should automatically receive metadata: None.
    wait_for_event(&mut viewer.events, Duration::from_secs(4), |ev| match ev {
        SfuEvent::PeerBroadcastUpdated {
            peer_id,
            metadata: None,
        } if peer_id == broadcaster_id => Some(()),
        _ => None,
    })
    .await?;

    let reason = broadcast
        .unexpected_stop_reason()
        .expect("unexpected stop reason should be populated");
    assert!(reason.contains("fechado"));

    let audio_cancel = observed_audio_cancel
        .lock()
        .clone()
        .expect("audio capturer should have received cancellation token");
    assert!(
        audio_cancel.is_cancelled(),
        "audio capture should be cancelled automatically when video capture ends"
    );
    assert!(broadcaster.video_store.latest_local_preview().is_none());

    broadcaster.receiver.stop_all();
    viewer.receiver.stop_all();
    broadcaster.client.disconnect();
    viewer.client.disconnect();
    broadcaster.cancel.cancel();
    viewer.cancel.cancel();
    server_task.abort();

    Ok(())
}
