use crate::config::AppConfig;
use crate::media::{
    BroadcastStats, LocalBroadcastSession, PeerAudioMixer, PeerMediaReceiver, PeerVideoStore,
};
use crate::net::{ConnectOutcome, ConnectRequest, SfuClient, SfuEvent, connect_sfu};
use crate::ui::lobby::{LobbyEntities, TofuPromptState, render_lobby};
use crate::ui::picker_modal::{PickerModalEvent, PickerModalState, open_picker_dialog};
use crate::ui::stage::render_stage;
use crate::ui::video_surface::{VideoSlot, VideoTileKey};
use gpui_kit::component::slider::{SliderEvent, SliderState};
use gpui_kit::component::{Theme, ThemeMode};
use gpui_kit::{
    AppContext as _, Context, Entity, FocusHandle, InteractiveElement as _, IntoElement,
    KeyDownEvent, ParentElement, Render, RenderImage, Styled, Subscription, Window, div,
};
use shared_protocol::{PeerId, PeerInfo, StreamConfig};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppScreen {
    Lobby,
    Stage,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StageLayout {
    Grid,
    Focus(VideoTileKey),
}

#[derive(Clone)]
pub struct PeerAudioUiState {
    pub slider: Entity<SliderState>,
    pub volume_pct: u32,
    pub muted: bool,
    pub _sub: Rc<Subscription>,
}

/// Root GPUI view coordinating Lobby, TOFU TLS verification, Capture Picker, and Stage.
pub struct AppView {
    tokio_rt: Arc<tokio::runtime::Runtime>,
    pub config: AppConfig,
    pub screen: AppScreen,
    pub lobby_entities: LobbyEntities,
    _lobby_subs: Vec<Subscription>,
    pub tofu_prompt: Option<TofuPromptState>,
    pub is_connecting: bool,
    pending_create_room: bool,
    pending_room_id: String,
    pending_room_password: Option<String>,
    pub status_message: Option<(String, bool)>,
    pub warning_banner: Option<String>,
    dismissed_warning: Option<String>,

    session_cancel: CancellationToken,
    pub video_store: Arc<PeerVideoStore>,
    pub audio_mixer: PeerAudioMixer,
    pub media_receiver: Arc<PeerMediaReceiver>,
    sfu_client: Option<SfuClient>,
    event_rx: Option<mpsc::UnboundedReceiver<SfuEvent>>,
    frame_notify_flag: Arc<AtomicBool>,

    pub local_broadcast: Option<LocalBroadcastSession>,
    pub local_broadcast_label: Option<String>,
    pub show_local_preview: bool,
    pub room_id: String,
    pub local_peer_id: Option<PeerId>,
    pub peers: Vec<PeerInfo>,
    pub subscribed_peers: HashSet<PeerId>,
    pub peer_sliders: HashMap<PeerId, PeerAudioUiState>,
    pub video_slots: HashMap<PeerId, VideoSlot>,
    pub local_preview_slot: VideoSlot,
    stale_images: Vec<Arc<RenderImage>>,

    pub stage_layout: StageLayout,
    pub is_fullscreen: bool,
    pub show_stats: bool,
    pub rtt_ms: Option<u32>,
    pub broadcast_stats: Option<BroadcastStats>,
    pub last_clipboard_feedback: Option<Instant>,
    picker_state: Option<(Entity<PickerModalState>, Subscription)>,
    connect_generation: u64,
    pump_active: bool,
    focus_handle: FocusHandle,
}

impl AppView {
    pub fn new(
        tokio_rt: Arc<tokio::runtime::Runtime>,
        config: AppConfig,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let (lobby_entities, lobby_subs) = LobbyEntities::new(&config, window, cx);
        let session_cancel = CancellationToken::new();
        let video_store = PeerVideoStore::new();
        let audio_mixer = PeerAudioMixer::new(session_cancel.clone());
        let media_receiver =
            PeerMediaReceiver::new(Arc::clone(&video_store), audio_mixer.clone());
        let frame_notify_flag = Arc::new(AtomicBool::new(false));
        let notify_flag = Arc::clone(&frame_notify_flag);
        media_receiver.set_frame_notifier(Arc::new(move || {
            notify_flag.store(true, Ordering::Release);
        }));

        Self {
            tokio_rt,
            config,
            screen: AppScreen::Lobby,
            lobby_entities,
            _lobby_subs: lobby_subs,
            tofu_prompt: None,
            is_connecting: false,
            pending_create_room: false,
            pending_room_id: String::new(),
            pending_room_password: None,
            status_message: None,
            warning_banner: None,
            dismissed_warning: None,
            session_cancel,
            video_store,
            audio_mixer,
            media_receiver,
            sfu_client: None,
            event_rx: None,
            frame_notify_flag,
            local_broadcast: None,
            local_broadcast_label: None,
            show_local_preview: false,
            room_id: String::new(),
            local_peer_id: None,
            peers: Vec::new(),
            subscribed_peers: HashSet::new(),
            peer_sliders: HashMap::new(),
            video_slots: HashMap::new(),
            local_preview_slot: VideoSlot::default(),
            stale_images: Vec::new(),
            stage_layout: StageLayout::Grid,
            is_fullscreen: false,
            show_stats: false,
            rtt_ms: None,
            broadcast_stats: None,
            last_clipboard_feedback: None,
            picker_state: None,
            connect_generation: 0,
            pump_active: false,
            focus_handle: cx.focus_handle(),
        }
    }

    pub fn toggle_theme(&mut self, dark: bool, cx: &mut Context<Self>) {
        self.config.dark_mode = dark;
        let _ = self.config.save();
        let mode = if dark {
            ThemeMode::Dark
        } else {
            ThemeMode::Light
        };
        Theme::change(mode, None, cx);
        cx.notify();
    }

    pub fn begin_connect(&mut self, create_room: bool, cx: &mut Context<Self>) {
        if self.is_connecting {
            return;
        }
        let server_addr = self
            .lobby_entities
            .server_addr
            .read(cx)
            .value()
            .trim()
            .to_string();
        let nickname = self
            .lobby_entities
            .nickname
            .read(cx)
            .value()
            .trim()
            .to_string();
        let token_raw = self
            .lobby_entities
            .auth_token
            .read(cx)
            .value()
            .trim()
            .to_string();
        let room_id = self
            .lobby_entities
            .room_id
            .read(cx)
            .value()
            .trim()
            .to_string();
        let pass_raw = self.lobby_entities.room_password.read(cx).value().to_string();

        if server_addr.is_empty() {
            self.status_message = Some(("Informe o endereço do servidor (host:porta).".into(), true));
            cx.notify();
            return;
        }
        if nickname.is_empty() {
            self.status_message = Some(("Informe seu apelido.".into(), true));
            cx.notify();
            return;
        }
        if !create_room && room_id.is_empty() {
            self.status_message = Some(("Informe o ID da sala para entrar.".into(), true));
            cx.notify();
            return;
        }

        let auth_token = (!token_raw.is_empty()).then_some(token_raw);
        let password = (!pass_raw.is_empty()).then_some(pass_raw);

        self.config.server_addr = server_addr.clone();
        self.config.nickname = nickname.clone();
        self.config.auth_token = auth_token.clone();
        let _ = self.config.save();

        self.pending_create_room = create_room;
        self.pending_room_id = room_id;
        self.pending_room_password = password;
        self.tofu_prompt = None;
        self.is_connecting = true;
        self.status_message = Some(("Conectando ao servidor...".into(), false));
        self.connect_generation = self.connect_generation.wrapping_add(1);
        let gen = self.connect_generation;

        let pinned = self.config.pinned_fingerprint(&server_addr).map(str::to_string);
        let req = ConnectRequest {
            server_addr,
            nickname,
            auth_token,
            pinned_fingerprint: pinned,
        };

        let (event_tx, event_rx) = mpsc::unbounded_channel::<SfuEvent>();
        self.event_rx = Some(event_rx);
        self.ensure_pump_started(cx);

        let rt = Arc::clone(&self.tokio_rt);
        let sink = Arc::clone(&self.media_receiver);
        let conn_task = rt.spawn(async move { connect_sfu(req, event_tx, sink).await });

        cx.spawn(async move |this, cx| {
            let outcome = conn_task.await;
            let _ = this.update(cx, |app, cx| {
                if app.connect_generation != gen {
                    return;
                }
                match outcome {
                    Ok(Ok(ConnectOutcome::Connected(sfu))) => {
                        app.is_connecting = false;
                        app.media_receiver.attach_sfu(Some(sfu.clone()));
                        app.local_peer_id = Some(sfu.local_peer_id());
                        if app.pending_create_room {
                            sfu.create_room(app.pending_room_password.clone());
                        } else {
                            sfu.join_room(
                                app.pending_room_id.clone(),
                                app.pending_room_password.clone(),
                            );
                        }
                        app.sfu_client = Some(sfu);
                        app.status_message = Some(("Entrando na sala...".into(), false));
                    }
                    Ok(Ok(ConnectOutcome::TrustRequired {
                        server_addr,
                        fingerprint,
                        changed_from,
                    })) => {
                        app.is_connecting = false;
                        app.status_message = None;
                        app.tofu_prompt = Some(TofuPromptState {
                            server_addr,
                            fingerprint,
                            changed_from,
                            create_room: app.pending_create_room,
                        });
                    }
                    Ok(Err(err)) => {
                        app.is_connecting = false;
                        app.status_message = Some((format!("Falha na conexão: {err}"), true));
                    }
                    Err(join_err) => {
                        app.is_connecting = false;
                        app.status_message =
                            Some((format!("Erro interno de conexão: {join_err}"), true));
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    pub fn cancel_tofu(&mut self, cx: &mut Context<Self>) {
        self.tofu_prompt = None;
        self.status_message = Some(("Conexão cancelada pelo usuário.".into(), false));
        cx.notify();
    }

    pub fn confirm_tofu_and_connect(&mut self, cx: &mut Context<Self>) {
        let Some(tofu) = self.tofu_prompt.take() else {
            return;
        };
        self.config
            .trust_fingerprint(&tofu.server_addr, &tofu.fingerprint);
        let _ = self.config.save();
        self.begin_connect(tofu.create_room, cx);
    }

    pub fn open_capture_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let modal = open_picker_dialog(&self.config, window, cx);
        let sub = cx.subscribe(&modal, |this, _, ev: &PickerModalEvent, cx| match ev {
            PickerModalEvent::Start { config, label } => {
                this.picker_state = None;
                this.start_local_broadcast(config.clone(), label.clone(), cx);
            }
            PickerModalEvent::Cancelled => {
                this.picker_state = None;
            }
        });
        self.picker_state = Some((modal, sub));
    }

    fn start_local_broadcast(
        &mut self,
        stream_cfg: StreamConfig,
        source_label: String,
        cx: &mut Context<Self>,
    ) {
        let Some(sfu) = self.sfu_client.clone() else {
            return;
        };
        if let Some(old) = self.local_broadcast.take() {
            old.stop(&sfu);
        }

        self.config.resolution = stream_cfg.resolution;
        self.config.fps = stream_cfg.sanitized_fps();
        self.config.bitrate = stream_cfg.bitrate;
        let _ = self.config.save();

        self.show_local_preview = false;
        self.video_store.clear_local_preview();
        self.dismissed_warning = None;
        let _rt_guard = self.tokio_rt.enter();
        let flag = Arc::clone(&self.frame_notify_flag);
        let preview_cb = Arc::new(move || {
            flag.store(true, Ordering::Release);
        });
        match LocalBroadcastSession::start(
            sfu,
            stream_cfg,
            Arc::clone(&self.video_store),
            Some(preview_cb),
        ) {
            Ok(session) => {
                session.set_local_preview_enabled(false);
                self.broadcast_stats = Some(session.stats_snapshot());
                self.local_broadcast = Some(session);
                self.local_broadcast_label = Some(source_label);
                self.ensure_pump_started(cx);
            }
            Err(err) => {
                self.local_broadcast_label = None;
                self.warning_banner = Some(format!("Erro ao iniciar captura: {err}"));
            }
        }
        cx.notify();
    }

    pub fn toggle_local_preview(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.local_broadcast.is_none() {
            return;
        }
        self.show_local_preview = !self.show_local_preview;
        if let Some(session) = &self.local_broadcast {
            session.set_local_preview_enabled(self.show_local_preview);
        }
        if !self.show_local_preview {
            self.video_store.clear_local_preview();
            self.local_preview_slot.clear(window);
            if matches!(self.stage_layout, StageLayout::Focus(VideoTileKey::LocalPreview)) {
                self.stage_layout = StageLayout::Grid;
            }
            if self.is_fullscreen && self.subscribed_peers.is_empty() {
                self.is_fullscreen = false;
            }
        }
        cx.notify();
    }

    pub fn stop_local_broadcast(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let (Some(session), Some(sfu)) = (self.local_broadcast.take(), self.sfu_client.as_ref()) {
            session.stop(sfu);
        }
        self.show_local_preview = false;
        self.local_broadcast_label = None;
        self.broadcast_stats = None;
        self.video_store.clear_local_preview();
        self.local_preview_slot.clear(window);
        if matches!(self.stage_layout, StageLayout::Focus(VideoTileKey::LocalPreview)) {
            self.stage_layout = StageLayout::Grid;
        }
        cx.notify();
    }

    pub fn toggle_peer_subscription(
        &mut self,
        peer_id: PeerId,
        subscribe: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(sfu) = self.sfu_client.as_ref() else {
            return;
        };
        if subscribe {
            self.subscribed_peers.insert(peer_id);
            sfu.set_subscription(peer_id, true, true);
            sfu.request_keyframe(peer_id);
        } else {
            self.subscribed_peers.remove(&peer_id);
            sfu.set_subscription(peer_id, false, false);
            self.media_receiver.stop_peer(peer_id);
            if let Some(mut slot) = self.video_slots.remove(&peer_id) {
                slot.clear(window);
            }
            if matches!(self.stage_layout, StageLayout::Focus(VideoTileKey::Remote(id)) if id == peer_id)
            {
                self.stage_layout = StageLayout::Grid;
            }
        }
        cx.notify();
    }

    pub fn toggle_peer_mute(&mut self, peer_id: PeerId, cx: &mut Context<Self>) {
        if let Some(ui) = self.peer_sliders.get_mut(&peer_id) {
            ui.muted = !ui.muted;
            self.audio_mixer.set_peer_muted(peer_id, ui.muted);
            cx.notify();
        }
    }

    fn ensure_peer_slider(&mut self, peer_id: PeerId, cx: &mut Context<Self>) {
        if Some(peer_id) == self.local_peer_id || self.peer_sliders.contains_key(&peer_id) {
            return;
        }
        let slider = cx.new(|_| {
            SliderState::new()
                .min(0.0)
                .max(200.0)
                .step(5.0)
                .default_value(100.0)
        });
        let sub = cx.subscribe(&slider, move |this, _, ev: &SliderEvent, cx| {
            let (SliderEvent::Change(val) | SliderEvent::Release(val)) = ev;
            let pct = val.end().clamp(0.0, 200.0).round() as u32;
            if let Some(st) = this.peer_sliders.get_mut(&peer_id) {
                st.volume_pct = pct;
            }
            this.audio_mixer
                .set_peer_volume(peer_id, (pct as f32) / 100.0);
            cx.notify();
        });
        self.peer_sliders.insert(
            peer_id,
            PeerAudioUiState {
                slider,
                volume_pct: 100,
                muted: false,
                _sub: Rc::new(sub),
            },
        );
    }

    pub fn active_warning(&self) -> Option<String> {
        if let Some(w) = &self.warning_banner {
            return Some(w.clone());
        }
        if let Some(bs) = &self.broadcast_stats {
            if let Some(w) = &bs.warning {
                if self.dismissed_warning.as_ref() != Some(w) {
                    return Some(w.clone());
                }
            }
        }
        None
    }

    pub fn dismiss_warning(&mut self, msg: String, cx: &mut Context<Self>) {
        if self.warning_banner.as_ref() == Some(&msg) {
            self.warning_banner = None;
        } else {
            self.dismissed_warning = Some(msg);
        }
        cx.notify();
    }

    pub fn leave_room(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(sfu) = self.sfu_client.take() {
            if let Some(broadcast) = self.local_broadcast.take() {
                broadcast.stop(&sfu);
            }
            sfu.leave_room();
            sfu.disconnect();
        } else {
            self.local_broadcast = None;
        }
        self.reset_stage_state(Some(window), cx);
        self.status_message = None;
        cx.notify();
    }

    fn reset_stage_state(&mut self, window: Option<&mut Window>, _cx: &mut Context<Self>) {
        self.media_receiver.attach_sfu(None);
        self.media_receiver.stop_all();
        self.video_store.clear_local_preview();
        self.session_cancel.cancel();
        self.session_cancel = CancellationToken::new();
        self.audio_mixer = PeerAudioMixer::new(self.session_cancel.clone());
        self.media_receiver =
            PeerMediaReceiver::new(Arc::clone(&self.video_store), self.audio_mixer.clone());
        let flag = Arc::clone(&self.frame_notify_flag);
        self.media_receiver.set_frame_notifier(Arc::new(move || {
            flag.store(true, Ordering::Release);
        }));

        if let Some(win) = window {
            for (_, mut slot) in self.video_slots.drain() {
                slot.clear(win);
            }
            self.local_preview_slot.clear(win);
        } else {
            for (_, mut slot) in self.video_slots.drain() {
                if let Some(img) = slot.take_image() {
                    self.stale_images.push(img);
                }
            }
            if let Some(img) = self.local_preview_slot.take_image() {
                self.stale_images.push(img);
            }
        }

        self.screen = AppScreen::Lobby;
        self.room_id.clear();
        self.local_peer_id = None;
        self.peers.clear();
        self.subscribed_peers.clear();
        self.peer_sliders.clear();
        self.stage_layout = StageLayout::Grid;
        self.is_fullscreen = false;
        self.show_local_preview = false;
        self.local_broadcast_label = None;
        self.rtt_ms = None;
        self.broadcast_stats = None;
        self.warning_banner = None;
        self.dismissed_warning = None;
    }

    fn ensure_pump_started(&mut self, cx: &mut Context<Self>) {
        if self.pump_active {
            return;
        }
        self.pump_active = true;
        cx.spawn(async move |this, cx| {
            let mut last_stats_tick = Instant::now();
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(16))
                    .await;

                let keep_going = this.update(cx, |app, cx| {
                    let has_events = app.drain_sfu_events(cx);
                    let has_frames = app.frame_notify_flag.swap(false, Ordering::AcqRel);
                    let stats_interval = if app.local_broadcast.is_some() {
                        Duration::from_millis(250)
                    } else {
                        Duration::from_millis(500)
                    };
                    let periodic = last_stats_tick.elapsed() >= stats_interval;
                    if periodic {
                        last_stats_tick = Instant::now();
                        if let Some(sfu) = &app.sfu_client {
                            app.rtt_ms = Some(sfu.counters().rtt_ms);
                        }
                        if let Some(bc) = &app.local_broadcast {
                            app.broadcast_stats = Some(bc.stats_snapshot());
                        }
                    }

                    if has_events || has_frames || (periodic && app.screen == AppScreen::Stage) {
                        cx.notify();
                    }

                    let active = app.is_connecting
                        || app.sfu_client.is_some()
                        || app.event_rx.is_some()
                        || app.screen == AppScreen::Stage;
                    if !active {
                        app.pump_active = false;
                    }
                    active
                });

                match keep_going {
                    Ok(true) => {}
                    _ => break,
                }
            }
        })
        .detach();
    }

    fn drain_sfu_events(&mut self, cx: &mut Context<Self>) -> bool {
        let mut events = Vec::new();
        if let Some(rx) = &mut self.event_rx {
            while let Ok(ev) = rx.try_recv() {
                events.push(ev);
            }
        }
        if events.is_empty() {
            return false;
        }

        for ev in events {
            match ev {
                SfuEvent::TrustRequired { .. } | SfuEvent::Connected { .. } => {}
                SfuEvent::RoomJoined {
                    room_id,
                    local_peer_id,
                    peers,
                } => {
                    self.is_connecting = false;
                    self.status_message = None;
                    self.screen = AppScreen::Stage;
                    self.room_id = room_id;
                    self.local_peer_id = Some(local_peer_id);
                    self.peers = peers;
                    let ids: Vec<PeerId> = self.peers.iter().map(|p| p.peer_id).collect();
                    for id in ids {
                        self.ensure_peer_slider(id, cx);
                    }
                }
                SfuEvent::PeerJoined(peer) => {
                    let pid = peer.peer_id;
                    if let Some(existing) = self.peers.iter_mut().find(|p| p.peer_id == pid) {
                        *existing = peer;
                    } else {
                        self.peers.push(peer);
                    }
                    self.ensure_peer_slider(pid, cx);
                }
                SfuEvent::PeerLeft { peer_id } => {
                    self.peers.retain(|p| p.peer_id != peer_id);
                    self.subscribed_peers.remove(&peer_id);
                    self.peer_sliders.remove(&peer_id);
                    self.media_receiver.stop_peer(peer_id);
                    if matches!(self.stage_layout, StageLayout::Focus(VideoTileKey::Remote(id)) if id == peer_id)
                    {
                        self.stage_layout = StageLayout::Grid;
                    }
                }
                SfuEvent::PeerBroadcastUpdated { peer_id, metadata } => {
                    let stopped = metadata.is_none();
                    if let Some(p) = self.peers.iter_mut().find(|p| p.peer_id == peer_id) {
                        p.active_stream = metadata;
                    }
                    if stopped {
                        self.subscribed_peers.remove(&peer_id);
                        self.video_store.remove_peer(peer_id);
                        if matches!(self.stage_layout, StageLayout::Focus(VideoTileKey::Remote(id)) if id == peer_id)
                        {
                            self.stage_layout = StageLayout::Grid;
                        }
                    }
                }
                SfuEvent::ForceKeyframe => {}
                SfuEvent::RttUpdated { rtt_ms } => {
                    self.rtt_ms = Some(rtt_ms);
                }
                SfuEvent::ServerError { code, message } => {
                    self.is_connecting = false;
                    if self.screen == AppScreen::Lobby {
                        self.status_message = Some((format!("{code:?}: {message}"), true));
                        if let Some(sfu) = self.sfu_client.take() {
                            sfu.disconnect();
                        }
                    } else {
                        self.warning_banner = Some(format!("Erro do servidor ({code:?}): {message}"));
                    }
                }
                SfuEvent::Disconnected { reason } => {
                    self.is_connecting = false;
                    if let Some(bc) = self.local_broadcast.take() {
                        if let Some(sfu) = &self.sfu_client {
                            bc.stop(sfu);
                        }
                    }
                    self.sfu_client = None;
                    self.event_rx = None;
                    let was_stage = self.screen == AppScreen::Stage;
                    self.reset_stage_state(None, cx);
                    if was_stage || self.tofu_prompt.is_none() {
                        self.status_message =
                            Some((format!("Desconectado do servidor: {reason}"), true));
                    }
                }
            }
        }
        true
    }
}

impl Drop for AppView {
    fn drop(&mut self) {
        if let Some(sfu) = self.sfu_client.take() {
            if let Some(bc) = self.local_broadcast.take() {
                bc.stop(&sfu);
            }
            sfu.disconnect();
        }
        self.media_receiver.stop_all();
        self.session_cancel.cancel();
    }
}

impl Render for AppView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        for stale in self.stale_images.drain(..) {
            let _ = window.drop_image(stale);
        }
        let body = match self.screen {
            AppScreen::Lobby => render_lobby(self, window, cx).into_any_element(),
            AppScreen::Stage => render_stage(self, window, cx).into_any_element(),
        };

        div()
            .size_full()
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                if event.keystroke.key == "escape" && this.is_fullscreen {
                    this.is_fullscreen = false;
                    cx.notify();
                } else if event.keystroke.modifiers.control
                    && event.keystroke.modifiers.shift
                    && event.keystroke.key.eq_ignore_ascii_case("s")
                {
                    this.show_stats = !this.show_stats;
                    cx.notify();
                }
            }))
            .child(body)
    }
}
