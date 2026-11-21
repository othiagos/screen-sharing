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
    KeyDownEvent, MouseMoveEvent, ParentElement, Pixels, Point, Render, RenderImage, Styled,
    Subscription, Window, div,
};
use shared_protocol::{ControlErrorCode, PeerId, PeerInfo, StreamConfig};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

const FULLSCREEN_OVERLAY_IDLE_TIMEOUT: Duration = Duration::from_secs(5);

pub(crate) fn format_server_error(code: ControlErrorCode) -> &'static str {
    match code {
        ControlErrorCode::InvalidPassword => "Senha da sala incorreta.",
        ControlErrorCode::RoomNotFound => {
            "Sala não encontrada. Verifique o código e tente novamente."
        }
        ControlErrorCode::Unauthorized => {
            "Acesso negado pelo servidor. Verifique a chave de acesso em Opções avançadas."
        }
        ControlErrorCode::ProtocolMismatch => {
            "Versão incompatível com o servidor. Atualize o aplicativo."
        }
        ControlErrorCode::NotInRoom => "Você não está em uma sala no momento.",
        ControlErrorCode::BadRequest => {
            "Não foi possível completar o pedido. Verifique os dados informados."
        }
        ControlErrorCode::Internal => {
            "O servidor encontrou um problema temporário. Tente novamente."
        }
    }
}

pub(crate) fn is_unauthorized_error(raw: &str) -> bool {
    raw.to_ascii_lowercase().contains("unauthorized")
}

pub(crate) fn format_connection_error(raw: &str) -> &'static str {
    let lower = raw.to_ascii_lowercase();
    if lower.contains("protocolmismatch") {
        format_server_error(ControlErrorCode::ProtocolMismatch)
    } else if lower.contains("unauthorized") {
        format_server_error(ControlErrorCode::Unauthorized)
    } else if lower.contains("invalidpassword") {
        format_server_error(ControlErrorCode::InvalidPassword)
    } else if lower.contains("roomnotfound") {
        format_server_error(ControlErrorCode::RoomNotFound)
    } else if lower.contains("host:port") || lower.contains("invalid socket address") {
        "Endereço do servidor inválido. Use o formato exemplo.com:4433."
    } else if lower.contains("failed resolving")
        || lower.contains("no ip address resolved")
        || lower.contains("name or service not known")
        || lower.contains("nodename nor servname")
        || lower.contains("dns")
    {
        "Servidor não encontrado. Verifique o endereço digitado."
    } else if lower.contains("timed out") || lower.contains("timeout") {
        "O servidor não respondeu a tempo. Verifique sua conexão e tente novamente."
    } else if lower.contains("connection refused")
        || lower.contains("unreachable")
        || lower.contains("reset")
    {
        "Não foi possível alcançar o servidor. Verifique se ele está ligado."
    } else {
        "Não foi possível conectar ao servidor. Verifique o endereço e sua internet."
    }
}

pub(crate) fn format_disconnect_reason(raw: &str) -> &'static str {
    let lower = raw.to_ascii_lowercase();
    if lower.contains("timed out") || lower.contains("timeout") {
        "Conexão perdida: o servidor parou de responder."
    } else if lower.contains("application closed") || lower.contains("locally closed") {
        "Conexão encerrada pelo servidor."
    } else if lower.contains("reset") {
        "Conexão interrompida inesperadamente."
    } else {
        "Você foi desconectado do servidor."
    }
}

pub(crate) fn format_capture_start_error(raw: &str) -> &'static str {
    let lower = raw.to_ascii_lowercase();
    if lower.contains("not found") || lower.contains("source not found") {
        "Não foi possível encontrar a tela ou janela selecionada."
    } else if lower.contains("cancel") || lower.contains("no pipewire streams") {
        "Compartilhamento cancelado."
    } else {
        "Não foi possível iniciar o compartilhamento de tela."
    }
}

pub(crate) fn should_show_fullscreen_overlays(
    is_fullscreen: bool,
    last_activity: Instant,
    now: Instant,
) -> bool {
    !is_fullscreen
        || now.saturating_duration_since(last_activity) < FULLSCREEN_OVERLAY_IDLE_TIMEOUT
}

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
    pub show_advanced_lobby: bool,
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
    pub fullscreen_overlay_visible: bool,
    fullscreen_last_activity: Instant,
    fullscreen_last_mouse_pos: Option<Point<Pixels>>,
    pending_os_fullscreen: Option<(bool, Instant)>,
    force_exit_os_fullscreen: bool,
    pub show_stats: bool,
    pub rtt_ms: Option<u32>,
    pub broadcast_stats: Option<BroadcastStats>,
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
            show_advanced_lobby: false,
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
            fullscreen_overlay_visible: true,
            fullscreen_last_activity: Instant::now(),
            fullscreen_last_mouse_pos: None,
            pending_os_fullscreen: None,
            force_exit_os_fullscreen: false,
            show_stats: false,
            rtt_ms: None,
            broadcast_stats: None,
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
            self.status_message = Some(("Informe o endereço do servidor.".into(), true));
            cx.notify();
            return;
        }
        if nickname.is_empty() {
            self.status_message = Some(("Informe seu nome.".into(), true));
            cx.notify();
            return;
        }
        if !create_room && room_id.is_empty() {
            self.status_message = Some(("Informe o código da sala para entrar.".into(), true));
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
                        let raw = format!("{err:#}");
                        tracing::warn!("Falha na conexão com o servidor: {raw}");
                        if is_unauthorized_error(&raw) {
                            app.show_advanced_lobby = true;
                        }
                        app.status_message =
                            Some((format_connection_error(&raw).to_string(), true));
                    }
                    Err(join_err) => {
                        app.is_connecting = false;
                        tracing::warn!("Erro interno ao conectar ao servidor: {join_err}");
                        app.status_message = Some((
                            "Não foi possível conectar ao servidor. Tente novamente.".into(),
                            true,
                        ));
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
        self.status_message = Some(("Conexão cancelada.".into(), false));
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
                let raw = err.to_string();
                tracing::warn!("Erro ao iniciar captura local: {raw}");
                self.local_broadcast_label = None;
                self.warning_banner = Some(format_capture_start_error(&raw).to_string());
            }
        }
        cx.notify();
    }

    pub fn has_fullscreen_target(&self) -> bool {
        !self.subscribed_peers.is_empty()
            || (self.local_broadcast.is_some() && self.show_local_preview)
    }

    pub(crate) fn apply_os_fullscreen(&mut self, fullscreen: bool, window: &mut Window) {
        let current_target = self
            .pending_os_fullscreen
            .filter(|(_, ts)| ts.elapsed() < Duration::from_millis(800))
            .map(|(target, _)| target)
            .unwrap_or_else(|| window.is_fullscreen());
        if current_target != fullscreen {
            window.toggle_fullscreen();
            self.pending_os_fullscreen = Some((fullscreen, Instant::now()));
        }
        self.force_exit_os_fullscreen = false;
    }

    fn reveal_fullscreen_overlays(&mut self, reset_mouse_pos: bool) {
        self.fullscreen_last_activity = Instant::now();
        self.fullscreen_overlay_visible = true;
        if reset_mouse_pos {
            self.fullscreen_last_mouse_pos = None;
        }
    }

    pub fn handle_fullscreen_mouse_move(
        &mut self,
        pos: Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        if !self.is_fullscreen || self.fullscreen_last_mouse_pos == Some(pos) {
            return;
        }
        self.fullscreen_last_mouse_pos = Some(pos);
        let was_visible = self.fullscreen_overlay_visible;
        self.reveal_fullscreen_overlays(false);
        if !was_visible {
            cx.notify();
        }
    }

    pub fn set_fullscreen(
        &mut self,
        fullscreen: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let was_fullscreen = self.is_fullscreen;
        self.apply_os_fullscreen(fullscreen, window);
        self.is_fullscreen = fullscreen;
        if fullscreen && !was_fullscreen {
            self.reveal_fullscreen_overlays(true);
        } else if !fullscreen {
            self.fullscreen_overlay_visible = true;
            self.fullscreen_last_mouse_pos = None;
        }
        cx.notify();
    }

    pub fn toggle_fullscreen(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let next = !self.is_fullscreen;
        self.set_fullscreen(next, window, cx);
    }

    fn sync_os_fullscreen(&mut self, window: &mut Window) {
        let os_fullscreen = window.is_fullscreen();
        if let Some((target, started_at)) = self.pending_os_fullscreen {
            if os_fullscreen == target || started_at.elapsed() >= Duration::from_millis(800) {
                self.pending_os_fullscreen = None;
            } else {
                return;
            }
        }
        if self.screen != AppScreen::Stage
            || (!self.is_fullscreen && self.force_exit_os_fullscreen)
        {
            self.force_exit_os_fullscreen = false;
            if os_fullscreen {
                window.toggle_fullscreen();
                self.pending_os_fullscreen = Some((false, Instant::now()));
            }
            self.is_fullscreen = false;
            self.fullscreen_overlay_visible = true;
            self.fullscreen_last_mouse_pos = None;
            return;
        }
        if os_fullscreen != self.is_fullscreen {
            self.is_fullscreen = os_fullscreen;
            if os_fullscreen {
                self.reveal_fullscreen_overlays(true);
            } else {
                self.fullscreen_overlay_visible = true;
                self.fullscreen_last_mouse_pos = None;
            }
        }
    }

    fn clear_local_broadcast_state(&mut self, window: Option<&mut Window>) {
        self.show_local_preview = false;
        self.local_broadcast_label = None;
        self.broadcast_stats = None;
        self.video_store.clear_local_preview();
        if let Some(win) = window {
            self.local_preview_slot.clear(win);
        } else if let Some(img) = self.local_preview_slot.take_image() {
            self.stale_images.push(img);
        }
        if matches!(self.stage_layout, StageLayout::Focus(VideoTileKey::LocalPreview)) {
            self.stage_layout = StageLayout::Grid;
        }
    }

    fn handle_unexpected_broadcast_stop(&mut self, reason: String, cx: &mut Context<Self>) {
        if let Some(session) = self.local_broadcast.take() {
            if let Some(sfu) = self.sfu_client.as_ref() {
                session.stop(sfu);
            }
        }
        self.clear_local_broadcast_state(None);
        if self.is_fullscreen && self.subscribed_peers.is_empty() {
            self.is_fullscreen = false;
            self.force_exit_os_fullscreen = true;
        }
        self.warning_banner = Some(reason);
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
                self.set_fullscreen(false, window, cx);
                return;
            }
        }
        cx.notify();
    }

    pub fn stop_local_broadcast(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let (Some(session), Some(sfu)) = (self.local_broadcast.take(), self.sfu_client.as_ref()) {
            session.stop(sfu);
        }
        self.clear_local_broadcast_state(Some(window));
        if self.is_fullscreen && self.subscribed_peers.is_empty() {
            self.set_fullscreen(false, window, cx);
            return;
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
            self.apply_os_fullscreen(false, win);
            for (_, mut slot) in self.video_slots.drain() {
                slot.clear(win);
            }
            self.local_preview_slot.clear(win);
        } else {
            self.force_exit_os_fullscreen = self.is_fullscreen;
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
                    let mut unexpected_stopped = false;
                    if let Some(reason) = app
                        .local_broadcast
                        .as_ref()
                        .and_then(|bc| bc.unexpected_stop_reason())
                    {
                        app.handle_unexpected_broadcast_stop(reason, cx);
                        unexpected_stopped = true;
                    }
                    let mut overlay_changed = false;
                    if app.is_fullscreen {
                        let visible_now = should_show_fullscreen_overlays(
                            app.is_fullscreen,
                            app.fullscreen_last_activity,
                            Instant::now(),
                        );
                        if visible_now != app.fullscreen_overlay_visible {
                            app.fullscreen_overlay_visible = visible_now;
                            overlay_changed = true;
                        }
                    }
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

                    if has_events
                        || unexpected_stopped
                        || overlay_changed
                        || has_frames
                        || (periodic && app.screen == AppScreen::Stage)
                    {
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
                    tracing::warn!(?code, %message, "Erro retornado pelo servidor");
                    self.is_connecting = false;
                    let friendly = format_server_error(code).to_string();
                    if self.screen == AppScreen::Lobby {
                        if matches!(code, ControlErrorCode::Unauthorized) {
                            self.show_advanced_lobby = true;
                        }
                        self.status_message = Some((friendly, true));
                        if let Some(sfu) = self.sfu_client.take() {
                            sfu.disconnect();
                        }
                    } else {
                        self.warning_banner = Some(friendly);
                    }
                }
                SfuEvent::Disconnected { reason } => {
                    tracing::warn!(%reason, "Desconectado do servidor");
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
                            Some((format_disconnect_reason(&reason).to_string(), true));
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
        self.sync_os_fullscreen(window);
        let body = match self.screen {
            AppScreen::Lobby => render_lobby(self, window, cx).into_any_element(),
            AppScreen::Stage => render_stage(self, window, cx).into_any_element(),
        };

        div()
            .size_full()
            .track_focus(&self.focus_handle)
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _, cx| {
                this.handle_fullscreen_mouse_move(event.position, cx);
            }))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                if event.keystroke.key == "escape" && this.is_fullscreen {
                    this.set_fullscreen(false, window, cx);
                } else if event.keystroke.key.eq_ignore_ascii_case("f11")
                    && !event.keystroke.modifiers.control
                    && !event.keystroke.modifiers.alt
                    && !event.keystroke.modifiers.platform
                {
                    if this.is_fullscreen {
                        this.set_fullscreen(false, window, cx);
                    } else if this.screen == AppScreen::Stage && this.has_fullscreen_target() {
                        this.set_fullscreen(true, window, cx);
                    }
                } else if event.keystroke.modifiers.control
                    && event.keystroke.modifiers.shift
                    && event.keystroke.key.eq_ignore_ascii_case("s")
                {
                    this.show_stats = !this.show_stats;
                    if this.is_fullscreen {
                        this.reveal_fullscreen_overlays(false);
                    }
                    cx.notify();
                }
            }))
            .child(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fullscreen_overlay_visibility_timeout() {
        let t0 = Instant::now();
        // Outside fullscreen, overlays always stay visible regardless of elapsed idle time.
        assert!(should_show_fullscreen_overlays(
            false,
            t0,
            t0 + Duration::from_secs(30)
        ));

        // Inside fullscreen, overlays remain visible until 5s of inactivity.
        assert!(should_show_fullscreen_overlays(
            true,
            t0,
            t0 + Duration::from_millis(4_999)
        ));
        assert!(!should_show_fullscreen_overlays(
            true,
            t0,
            t0 + Duration::from_secs(5)
        ));
        assert!(!should_show_fullscreen_overlays(
            true,
            t0,
            t0 + Duration::from_secs(12)
        ));

        // New mouse activity resets the 5s window.
        let t_move = t0 + Duration::from_secs(12);
        assert!(should_show_fullscreen_overlays(
            true,
            t_move,
            t_move + Duration::from_secs(2)
        ));
        assert!(!should_show_fullscreen_overlays(
            true,
            t_move,
            t_move + Duration::from_secs(5)
        ));
    }

    #[test]
    fn test_humanized_server_connection_and_disconnect_errors() {
        let server_codes = [
            ControlErrorCode::ProtocolMismatch,
            ControlErrorCode::Unauthorized,
            ControlErrorCode::InvalidPassword,
            ControlErrorCode::RoomNotFound,
            ControlErrorCode::NotInRoom,
            ControlErrorCode::BadRequest,
            ControlErrorCode::Internal,
        ];
        for code in server_codes {
            let msg = format_server_error(code);
            assert!(!msg.is_empty());
            assert!(!msg.contains("SFU"));
            assert!(!msg.contains(&format!("{code:?}")));
        }

        assert_eq!(
            format_connection_error("SFU rejected Hello (Unauthorized): invalid token"),
            format_server_error(ControlErrorCode::Unauthorized)
        );
        assert!(is_unauthorized_error(
            "SFU rejected Hello (Unauthorized): invalid token"
        ));
        assert_eq!(
            format_connection_error("SFU rejected Hello (ProtocolMismatch): expected v4"),
            format_server_error(ControlErrorCode::ProtocolMismatch)
        );
        assert_eq!(
            format_connection_error("Server address must be in host:port format"),
            "Endereço do servidor inválido. Use o formato exemplo.com:4433."
        );
        assert_eq!(
            format_connection_error("Failed resolving SFU host 'bad.local:4433'"),
            "Servidor não encontrado. Verifique o endereço digitado."
        );
        assert_eq!(
            format_connection_error("QUIC TLS handshake failed: timed out"),
            "O servidor não respondeu a tempo. Verifique sua conexão e tente novamente."
        );
        assert_eq!(
            format_connection_error("unexpected low-level quinn error"),
            "Não foi possível conectar ao servidor. Verifique o endereço e sua internet."
        );

        assert_eq!(
            format_disconnect_reason("timed out"),
            "Conexão perdida: o servidor parou de responder."
        );
        assert_eq!(
            format_disconnect_reason("application closed: 0"),
            "Conexão encerrada pelo servidor."
        );
        assert_eq!(
            format_disconnect_reason("connection reset by peer"),
            "Conexão interrompida inesperadamente."
        );

        assert_eq!(
            format_capture_start_error("Source not found: Monitor id=1"),
            "Não foi possível encontrar a tela ou janela selecionada."
        );
        assert_eq!(
            format_capture_start_error("Portal returned no PipeWire streams"),
            "Compartilhamento cancelado."
        );
        assert_eq!(
            format_capture_start_error("xcap error"),
            "Não foi possível iniciar o compartilhamento de tela."
        );
    }
}
