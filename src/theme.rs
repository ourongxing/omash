use ratatui::style::Color;
use serde::Deserialize;
use std::{fs, path::PathBuf};

#[derive(Clone, Debug)]
pub struct Theme {
    pub accent: Color,
    pub muted: Color,
    pub border: Color,
    pub background: Color,
    pub surface: Color,
    pub surface_active: Color,
    pub foreground: Color,
    pub success: Color,
    pub warning: Color,
    pub danger: Color,
    pub info: Color,
    source: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct ThemeFile {
    pub(crate) accent: Option<String>,
    pub(crate) muted: Option<String>,
    pub(crate) border: Option<String>,
    pub(crate) background: Option<String>,
    pub(crate) surface: Option<String>,
    pub(crate) surface_active: Option<String>,
    pub(crate) foreground: Option<String>,
    pub(crate) success: Option<String>,
    pub(crate) warning: Option<String>,
    pub(crate) danger: Option<String>,
    pub(crate) info: Option<String>,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            accent: Color::Rgb(103, 210, 255),
            muted: Color::Rgb(118, 130, 151),
            border: Color::Rgb(55, 66, 84),
            background: Color::Rgb(14, 18, 25),
            surface: Color::Rgb(24, 29, 39),
            surface_active: Color::Rgb(35, 47, 63),
            foreground: Color::Rgb(255, 255, 255),
            success: Color::Rgb(91, 214, 151),
            warning: Color::Rgb(245, 190, 90),
            danger: Color::Rgb(244, 112, 122),
            info: Color::Cyan,
            source: None,
        }
    }
}

impl Theme {
    pub fn load() -> Self {
        let mut theme = Self::default();
        theme.refresh();
        theme
    }

    pub fn refresh(&mut self) {
        let standalone = fs::read_to_string(Self::path()).ok();
        let omarchy = standalone
            .is_none()
            .then(crate::omarchy::current_theme)
            .flatten();
        let (source, theme) = Self::select(standalone, omarchy);
        if source == self.source {
            return;
        }
        *self = theme;
        self.source = source;
    }

    pub fn path() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("omash/theme.toml")
    }

    fn select(
        standalone: Option<String>,
        omarchy: Option<(String, ThemeFile)>,
    ) -> (Option<String>, Self) {
        if let Some(source) = standalone {
            let theme = Self::from_source(&source).unwrap_or_default();
            (Some(format!("omash:{source}")), theme)
        } else if let Some((source, file)) = omarchy {
            (Some(format!("omarchy:{source}")), Self::from_file(file))
        } else {
            (None, Self::default())
        }
    }

    fn from_source(source: &str) -> Option<Self> {
        let file: ThemeFile = toml::from_str(source).ok()?;
        Some(Self::from_file(file))
    }

    pub(crate) fn from_file(file: ThemeFile) -> Self {
        let fallback = Self::default();
        Self {
            accent: color(file.accent.as_deref(), fallback.accent),
            muted: color(file.muted.as_deref(), fallback.muted),
            border: color(file.border.as_deref(), fallback.border),
            background: color(file.background.as_deref(), fallback.background),
            surface: color(file.surface.as_deref(), fallback.surface),
            surface_active: color(file.surface_active.as_deref(), fallback.surface_active),
            foreground: color(file.foreground.as_deref(), fallback.foreground),
            success: color(file.success.as_deref(), fallback.success),
            warning: color(file.warning.as_deref(), fallback.warning),
            danger: color(file.danger.as_deref(), fallback.danger),
            info: color(file.info.as_deref(), fallback.info),
            source: None,
        }
    }
}

fn color(value: Option<&str>, fallback: Color) -> Color {
    value.and_then(parse_hex).unwrap_or(fallback)
}

fn parse_hex(value: &str) -> Option<Color> {
    let hex = value.strip_prefix('#')?;
    if hex.len() != 6 {
        return None;
    }
    Some(Color::Rgb(
        u8::from_str_radix(&hex[0..2], 16).ok()?,
        u8::from_str_radix(&hex[2..4], 16).ok()?,
        u8::from_str_radix(&hex[4..6], 16).ok()?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_standalone_palette_to_ui_roles() {
        let theme = Theme::from_source(
            r##"
            accent = "#112233"
            surface_active = "#223344"
            border = "#334455"
            surface = "#445566"
            background = "#556677"
            foreground = "#667788"
            muted = "#778899"
            danger = "#881122"
            warning = "#998822"
            success = "#229944"
            info = "#228899"
            "##,
        )
        .unwrap();

        assert_eq!(theme.accent, Color::Rgb(0x11, 0x22, 0x33));
        assert_eq!(theme.surface, Color::Rgb(0x44, 0x55, 0x66));
        assert_eq!(theme.background, Color::Rgb(0x55, 0x66, 0x77));
        assert_eq!(theme.foreground, Color::Rgb(0x66, 0x77, 0x88));
        assert_eq!(theme.muted, Color::Rgb(0x77, 0x88, 0x99));
        assert_eq!(theme.success, Color::Rgb(0x22, 0x99, 0x44));
    }

    #[test]
    fn standalone_theme_takes_priority_over_omarchy() {
        let standalone = "accent = '#112233'".to_owned();
        let omarchy = ThemeFile {
            accent: Some("#445566".into()),
            ..ThemeFile::default()
        };
        let (_, theme) = Theme::select(Some(standalone), Some(("palette".into(), omarchy)));
        assert_eq!(theme.accent, Color::Rgb(0x11, 0x22, 0x33));
    }

    #[test]
    fn omarchy_is_the_fallback_when_no_standalone_theme_exists() {
        let omarchy = ThemeFile {
            accent: Some("#445566".into()),
            ..ThemeFile::default()
        };
        let (_, theme) = Theme::select(None, Some(("palette".into(), omarchy)));
        assert_eq!(theme.accent, Color::Rgb(0x44, 0x55, 0x66));
    }
}
