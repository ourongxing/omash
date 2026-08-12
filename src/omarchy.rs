use crate::theme::ThemeFile;
use serde::Deserialize;
use std::fs;

#[derive(Debug, Deserialize)]
struct OmarchyColors {
    accent: String,
    selection: String,
    muted: String,
    background: String,
    dark_background: String,
    foreground: String,
    light_foreground: String,
    red: String,
    yellow: String,
    green: String,
    cyan: String,
}

pub fn current_theme() -> Option<(String, ThemeFile)> {
    let path = dirs::home_dir()?.join(".local/state/omarchy/current/theme/colors.toml");
    let source = fs::read_to_string(path).ok()?;
    let colors: OmarchyColors = toml::from_str(&source).ok()?;
    Some((
        source,
        ThemeFile {
            accent: Some(colors.accent),
            muted: Some(colors.light_foreground),
            border: Some(colors.muted),
            background: Some(colors.dark_background),
            surface: Some(colors.background),
            surface_active: Some(colors.selection),
            foreground: Some(colors.foreground),
            success: Some(colors.green),
            warning: Some(colors.yellow),
            danger: Some(colors.red),
            info: Some(colors.cyan),
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_an_omarchy_palette() {
        let colors: OmarchyColors = toml::from_str(
            r##"
            accent = "#112233"
            selection = "#223344"
            muted = "#334455"
            background = "#445566"
            dark_background = "#556677"
            foreground = "#667788"
            light_foreground = "#778899"
            red = "#881122"
            yellow = "#998822"
            green = "#229944"
            cyan = "#228899"
            "##,
        )
        .unwrap();
        assert_eq!(colors.accent, "#112233");
        assert_eq!(colors.background, "#445566");
    }
}
