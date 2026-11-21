use rust_i18n::t;
use shared_protocol::{ThrottleReason, TEXT_FOCUS_FPS};

/// Supported application languages, with English (`en`) as default fallback.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AppLocale {
    #[default]
    En,
    Es,
    PtBr,
}

impl AppLocale {
    pub const ALL: [Self; 3] = [Self::En, Self::Es, Self::PtBr];

    /// Canonical BCP-47 locale tag matching `locales/<code >.yml`.
    pub fn code(self) -> &'static str {
        match self {
            Self::En => "en",
            Self::Es => "es",
            Self::PtBr => "pt-BR",
        }
    }

    /// Autonym displayed in the language selector so users can recognize their language immediately.
    pub fn native_name(self) -> &'static str {
        match self {
            Self::En => "English",
            Self::Es => "Español",
            Self::PtBr => "Português (BR)",
        }
    }

    pub fn index(self) -> usize {
        match self {
            Self::En => 0,
            Self::Es => 1,
            Self::PtBr => 2,
        }
    }

    pub fn from_index(idx: usize) -> Self {
        Self::ALL.get(idx).copied().unwrap_or(Self::En)
    }

    /// Maps an arbitrary OS or config locale tag (e.g. `"pt_BR.UTF-8"`, `"es-MX"`, `"en-US"`) to a supported locale.
    pub fn from_tag(tag: &str) -> Self {
        let normalized = tag
            .trim()
            .split('.')
            .next()
            .unwrap_or("")
            .replace('_', "-")
            .to_ascii_lowercase();
        if normalized == "pt" || normalized.starts_with("pt-") {
            Self::PtBr
        } else if normalized == "es" || normalized.starts_with("es-") {
            Self::Es
        } else {
            Self::En
        }
    }

    /// Parses an explicitly saved locale preference (returning `None` if unrecognized or blank).
    pub fn from_saved(tag: &str) -> Option<Self> {
        let normalized = tag.trim().replace('_', "-").to_ascii_lowercase();
        if normalized.is_empty() {
            return None;
        }
        if normalized == "en" || normalized.starts_with("en-") {
            Some(Self::En)
        } else if normalized == "es" || normalized.starts_with("es-") {
            Some(Self::Es)
        } else if normalized == "pt" || normalized.starts_with("pt-") {
            Some(Self::PtBr)
        } else {
            None
        }
    }

    /// Resolves the initial locale from saved `AppConfig` or the host OS locale (defaulting to English).
    pub fn from_saved_or_system(saved: Option<&str>) -> Self {
        if let Some(locale) = saved.and_then(Self::from_saved) {
            return locale;
        }
        sys_locale::get_locale()
            .as_deref()
            .map(Self::from_tag)
            .unwrap_or(Self::En)
    }
}

/// Applies `locale` to both `desktop-client` and `gpui-kit` components.
pub fn apply_locale(locale: AppLocale) {
    let code = locale.code();
    rust_i18n::set_locale(code);
    gpui_kit::component::set_locale(code);
}

/// Returns the currently active application locale.
pub fn current_locale() -> AppLocale {
    AppLocale::from_tag(&rust_i18n::locale())
}

/// Localized video mode label (`Sharp Text` / `Texto nítido` / `Texto Nítido` or `{fps} FPS`).
pub fn video_mode_label(fps: u8) -> String {
    if fps <= TEXT_FOCUS_FPS {
        t!("modes.sharp_text").into_owned()
    } else {
        format!("{fps} FPS")
    }
}

/// Localized technical throttle label for the on-demand Details overlay.
pub fn throttle_reason_label(reason: ThrottleReason) -> String {
    match reason {
        ThrottleReason::None => t!("details.throttle_normal").into_owned(),
        ThrottleReason::Network => t!("details.throttle_network").into_owned(),
        ThrottleReason::Viewer => t!("details.throttle_viewer").into_owned(),
        ThrottleReason::Cpu => t!("details.throttle_cpu").into_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, BTreeSet};

    fn parse_flat_yaml_keys(raw: &str) -> BTreeMap<String, String> {
        let mut out = BTreeMap::new();
        let mut section = String::new();
        for (line_no, line) in raw.lines().enumerate() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            let indent = line.len() - line.trim_start().len();
            if indent == 0 {
                let sec = trimmed
                    .strip_suffix(':')
                    .unwrap_or_else(|| panic!("expected section header on line {}", line_no + 1));
                section = sec.trim().to_string();
            } else {
                let (key, val) = trimmed
                    .split_once(':')
                    .unwrap_or_else(|| panic!("expected key: value on line {}", line_no + 1));
                let full_key = format!("{section}.{}", key.trim());
                let val_clean = val.trim().trim_matches('"').to_string();
                assert!(
                    !val_clean.is_empty(),
                    "empty translation value for key '{full_key}'"
                );
                assert!(
                    out.insert(full_key.clone(), val_clean).is_none(),
                    "duplicate translation key '{full_key}'"
                );
            }
        }
        out
    }

    fn extract_placeholders(value: &str) -> BTreeSet<String> {
        let mut placeholders = BTreeSet::new();
        let mut rest = value;
        while let Some(start) = rest.find("%{") {
            let after = &rest[start + 2..];
            if let Some(end) = after.find('}') {
                placeholders.insert(after[..end].to_string());
                rest = &after[end + 1..];
            } else {
                break;
            }
        }
        placeholders
    }

    #[test]
    fn test_locale_tag_normalization_and_fallback() {
        assert_eq!(AppLocale::from_tag("en"), AppLocale::En);
        assert_eq!(AppLocale::from_tag("en-US"), AppLocale::En);
        assert_eq!(AppLocale::from_tag("en_GB.UTF-8"), AppLocale::En);
        assert_eq!(AppLocale::from_tag("es"), AppLocale::Es);
        assert_eq!(AppLocale::from_tag("es-MX"), AppLocale::Es);
        assert_eq!(AppLocale::from_tag("es_AR.UTF-8"), AppLocale::Es);
        assert_eq!(AppLocale::from_tag("pt"), AppLocale::PtBr);
        assert_eq!(AppLocale::from_tag("pt-BR"), AppLocale::PtBr);
        assert_eq!(AppLocale::from_tag("pt_PT.UTF-8"), AppLocale::PtBr);
        assert_eq!(AppLocale::from_tag("fr-FR"), AppLocale::En);
        assert_eq!(AppLocale::from_tag(""), AppLocale::En);

        assert_eq!(AppLocale::from_saved("pt-BR"), Some(AppLocale::PtBr));
        assert_eq!(AppLocale::from_saved("es"), Some(AppLocale::Es));
        assert_eq!(AppLocale::from_saved("en"), Some(AppLocale::En));
        assert_eq!(AppLocale::from_saved("unknown"), None);
        assert_eq!(AppLocale::from_saved(""), None);
    }

    #[test]
    fn test_all_locale_files_have_identical_keys_and_placeholders() {
        let en = parse_flat_yaml_keys(include_str!("../locales/en.yml"));
        let es = parse_flat_yaml_keys(include_str!("../locales/es.yml"));
        let pt_br = parse_flat_yaml_keys(include_str!("../locales/pt-BR.yml"));

        let en_keys: BTreeSet<&String> = en.keys().collect();
        let es_keys: BTreeSet<&String> = es.keys().collect();
        let pt_keys: BTreeSet<&String> = pt_br.keys().collect();

        assert!(!en_keys.is_empty());
        assert_eq!(en_keys, es_keys, "es.yml keys must match en.yml 100%");
        assert_eq!(en_keys, pt_keys, "pt-BR.yml keys must match en.yml 100%");

        for (key, en_val) in &en {
            let en_ph = extract_placeholders(en_val);
            let es_ph = extract_placeholders(&es[key]);
            let pt_ph = extract_placeholders(&pt_br[key]);
            assert_eq!(
                en_ph, es_ph,
                "es.yml placeholders mismatch for key '{key}'"
            );
            assert_eq!(
                en_ph, pt_ph,
                "pt-BR.yml placeholders mismatch for key '{key}'"
            );
        }
    }

    #[test]
    fn test_translations_resolve_in_en_es_pt_br() {
        assert_eq!(t!("modes.sharp_text", locale = "en"), "Sharp Text");
        assert_eq!(t!("modes.sharp_text", locale = "es"), "Texto nítido");
        assert_eq!(t!("modes.sharp_text", locale = "pt-BR"), "Texto Nítido");

        assert_eq!(t!("lobby.join_room", locale = "en"), "Join Room");
        assert_eq!(t!("lobby.join_room", locale = "es"), "Unirse a la sala");
        assert_eq!(t!("lobby.join_room", locale = "pt-BR"), "Entrar na Sala");

        assert_eq!(t!("stage.details", locale = "en"), "Details");
        assert_eq!(t!("stage.details", locale = "es"), "Detalles");
        assert_eq!(t!("stage.details", locale = "pt-BR"), "Detalhes");

        assert_eq!(
            t!("errors.invalid_password", locale = "en"),
            "Incorrect room password."
        );
        assert_eq!(
            t!("errors.invalid_password", locale = "es"),
            "Contraseña de la sala incorrecta."
        );
        assert_eq!(
            t!("errors.invalid_password", locale = "pt-BR"),
            "Senha da sala incorreta."
        );

        for loc in ["en", "es", "pt-BR"] {
            assert_eq!(
                t!("lobby.room_code_placeholder", locale = loc),
                "a1b2c3d4",
                "room code placeholder must match generated 8-char hex format for {loc}"
            );
        }
    }
}
