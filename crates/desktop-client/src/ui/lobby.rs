use crate::ui::app_view::AppView;
use gpui_kit::component::accordion::Accordion;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{ActiveTheme, Disableable as _, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    App, AppContext as _, Context, Entity, IntoElement, ParentElement, SharedString, Styled,
    Window, div, hsla, px,
};

/// Input entities preserved across renders on the Lobby screen.
pub struct LobbyEntities {
    pub server_addr: Entity<InputState>,
    pub nickname: Entity<InputState>,
    pub auth_token: Entity<InputState>,
    pub room_id: Entity<InputState>,
    pub room_password: Entity<InputState>,
}

impl LobbyEntities {
    pub fn new(
        config: &crate::config::AppConfig,
        window: &mut Window,
        cx: &mut Context<AppView>,
    ) -> (Self, Vec<gpui_kit::Subscription>) {
        let addr_default = config.server_addr.clone();
        let nick_default = config.nickname.clone();
        let token_default = config.auth_token.clone().unwrap_or_default();

        let server_addr = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("exemplo.com:4433")
                .default_value(addr_default)
        });
        let nickname = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Como você quer aparecer")
                .default_value(nick_default)
        });
        let auth_token = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Deixe em branco se não for necessária")
                .default_value(token_default)
                .masked(true)
        });
        let room_id = cx.new(|cx| {
            InputState::new(window, cx).placeholder("Ex: sala-principal")
        });
        let room_password = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Opcional")
                .masked(true)
        });

        let sub_room = cx.subscribe(&room_id, |this: &mut AppView, _, ev: &InputEvent, cx| {
            if matches!(ev, InputEvent::PressEnter { .. }) {
                this.begin_connect(false, cx);
            }
        });
        let sub_pass = cx.subscribe(
            &room_password,
            |this: &mut AppView, _, ev: &InputEvent, cx| {
                if matches!(ev, InputEvent::PressEnter { .. }) {
                    this.begin_connect(false, cx);
                }
            },
        );

        (
            Self {
                server_addr,
                nickname,
                auth_token,
                room_id,
                room_password,
            },
            vec![sub_room, sub_pass],
        )
    }
}

/// Pending TOFU (Trust-On-First-Use) TLS fingerprint prompt displayed before authenticating.
#[derive(Debug, Clone)]
pub struct TofuPromptState {
    pub server_addr: String,
    pub fingerprint: String,
    pub changed_from: Option<String>,
    pub create_room: bool,
}

fn labeled_field(label: &'static str, input: &Entity<InputState>, cx: &App) -> impl IntoElement {
    v_flex()
        .gap_1()
        .child(
            div()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(label),
        )
        .child(Input::new(input))
}

pub fn render_lobby(
    app: &mut AppView,
    _window: &mut Window,
    cx: &mut Context<AppView>,
) -> impl IntoElement {
    let bg = cx.theme().background;
    let card_bg = cx.theme().secondary;
    let border = cx.theme().border;
    let fg = cx.theme().foreground;
    let muted = cx.theme().muted_foreground;
    let danger = cx.theme().danger;
    let accent = cx.theme().accent;

    let is_dark = app.config.dark_mode;
    let is_connecting = app.is_connecting;
    let status_banner = app.status_message.clone();
    let tofu_opt = app.tofu_prompt.clone();

    let center_card = if let Some(tofu) = tofu_opt {
        let changed = tofu.changed_from.is_some();
        v_flex()
            .w(px(540.))
            .p_6()
            .gap_4()
            .rounded(cx.theme().radius_lg)
            .bg(card_bg)
            .border_1()
            .border_color(if changed { danger } else { accent })
            .child(
                div()
                    .text_lg()
                    .font_weight(gpui_kit::FontWeight::BOLD)
                    .text_color(if changed { danger } else { fg })
                    .child(if changed {
                        "Atenção: a identidade deste servidor mudou"
                    } else {
                        "Primeira conexão com este servidor"
                    }),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(muted)
                    .child(if changed {
                        format!(
                            "O servidor {} apresentou uma identificação diferente da última vez. Só continue se souber que ele foi atualizado ou reinstalado.",
                            tofu.server_addr
                        )
                    } else {
                        format!(
                            "Antes de conectar a {}, confirme que você confia neste endereço.",
                            tofu.server_addr
                        )
                    }),
            )
            .when_some(tofu.changed_from.clone(), |el, old_fp| {
                el.child(
                    v_flex()
                        .p_3()
                        .gap_1()
                        .rounded(cx.theme().radius)
                        .bg(hsla(0.0, 0.7, 0.4, 0.16))
                        .border_1()
                        .border_color(danger)
                        .child(
                            div()
                                .text_xs()
                                .text_color(danger)
                                .child("Código anterior"),
                        )
                        .child(div().text_xs().text_color(fg).child(old_fp)),
                )
            })
            .child(
                v_flex()
                    .p_3()
                    .gap_1()
                    .rounded(cx.theme().radius)
                    .bg(bg)
                    .border_1()
                    .border_color(border)
                    .child(
                        div()
                            .text_xs()
                            .text_color(muted)
                            .child(if changed {
                                "Código atual"
                            } else {
                                "Código de verificação"
                            }),
                    )
                    .child(div().text_xs().text_color(fg).child(tofu.fingerprint.clone())),
            )
            .child(
                h_flex()
                    .justify_end()
                    .gap_3()
                    .child(
                        Button::new("tofu-cancel")
                            .outline()
                            .label("Cancelar")
                            .on_click(cx.listener(|this, _, _, cx| this.cancel_tofu(cx))),
                    )
                    .child(
                        Button::new("tofu-trust")
                            .primary()
                            .label("Confiar e Conectar")
                            .on_click(cx.listener(|this, _, _, cx| this.confirm_tofu_and_connect(cx))),
                    ),
            )
    } else {
        let app_handle = cx.entity().downgrade();

        v_flex()
            .w(px(500.))
            .p_6()
            .gap_4()
            .rounded(cx.theme().radius_lg)
            .bg(card_bg)
            .border_1()
            .border_color(border)
            .child(
                h_flex()
                    .justify_between()
                    .items_center()
                    .child(
                        v_flex()
                            .gap_0p5()
                            .child(
                                div()
                                    .text_xl()
                                    .font_weight(gpui_kit::FontWeight::BOLD)
                                    .text_color(fg)
                                    .child("Screen Share"),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(muted)
                                    .child("Compartilhe sua tela e áudio com a sala"),
                            ),
                    )
                    .child(
                        Switch::new("theme-toggle")
                            .checked(is_dark)
                            .label("Escuro")
                            .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                this.toggle_theme(*checked, cx);
                            })),
                    ),
            )
            .child(labeled_field(
                "Servidor",
                &app.lobby_entities.server_addr,
                cx,
            ))
            .child(labeled_field(
                "Seu nome",
                &app.lobby_entities.nickname,
                cx,
            ))
            .child(
                h_flex()
                    .gap_3()
                    .child(div().flex_1().child(labeled_field(
                        "Código da sala",
                        &app.lobby_entities.room_id,
                        cx,
                    )))
                    .child(div().flex_1().child(labeled_field(
                        "Senha da sala (opcional)",
                        &app.lobby_entities.room_password,
                        cx,
                    ))),
            )
            .child(
                Accordion::new("lobby-advanced-accordion")
                    .w_full()
                    .h_auto()
                    .small()
                    .on_toggle_click(move |open_indices, _, cx| {
                        let is_open = open_indices.contains(&0);
                        let _ = app_handle.update(cx, |this, cx| {
                            if this.show_advanced_lobby != is_open {
                                this.show_advanced_lobby = is_open;
                                cx.notify();
                            }
                        });
                    })
                    .item(|item| {
                        item.open(app.show_advanced_lobby)
                            .title(
                                div()
                                    .text_xs()
                                    .text_color(if app.show_advanced_lobby { fg } else { muted })
                                    .child("Opções avançadas"),
                            )
                            .child(div().pt_1().child(labeled_field(
                                "Chave de acesso do servidor (opcional)",
                                &app.lobby_entities.auth_token,
                                cx,
                            )))
                    }),
            )
            .when_some(status_banner, |el, (msg, is_err)| {
                let banner_color = if is_err { danger } else { accent };
                let banner_bg = if is_err {
                    hsla(0.0, 0.7, 0.4, 0.16)
                } else {
                    hsla(0.58, 0.7, 0.45, 0.14)
                };
                el.child(
                    div()
                        .p_2p5()
                        .rounded(cx.theme().radius)
                        .bg(banner_bg)
                        .border_1()
                        .border_color(banner_color)
                        .text_xs()
                        .text_color(if is_err { danger } else { fg })
                        .child(SharedString::from(msg)),
                )
            })
            .child(
                h_flex()
                    .gap_3()
                    .pt_1()
                    .child(
                        div().flex_1().child(
                            Button::new("btn-join-room")
                                .primary()
                                .label(if is_connecting {
                                    "Conectando..."
                                } else {
                                    "Entrar na Sala"
                                })
                                .disabled(is_connecting)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.begin_connect(false, cx);
                                })),
                        ),
                    )
                    .child(
                        div().flex_1().child(
                            Button::new("btn-create-room")
                                .outline()
                                .label("Criar Nova Sala")
                                .disabled(is_connecting)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.begin_connect(true, cx);
                                })),
                        ),
                    ),
            )
    };

    div()
        .size_full()
        .bg(bg)
        .flex()
        .items_center()
        .justify_center()
        .p_6()
        .child(center_card)
}
