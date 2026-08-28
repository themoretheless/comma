//! Runtime configuration (`~/.comma.toml`) plus central UI constants.

use alacritty_terminal::vte::ansi::Rgb;
use egui::Color32;
use serde::Deserialize;

/// Terminal font size in points.
pub const FONT_SIZE: f32 = 14.0;

/// Cell size guess used before fonts are measured on the first frame.
pub const DEFAULT_CELL_WIDTH: f32 = 8.0;
pub const DEFAULT_CELL_HEIGHT: f32 = 17.0;

/// Initial window size in points.
pub const WINDOW_WIDTH: f32 = 1100.0;
pub const WINDOW_HEIGHT: f32 = 700.0;

/// Width of the tab sidebar.
pub const SIDEBAR_WIDTH: f32 = 160.0;

/// Scrollback history in lines (alacritty's default).
pub const SCROLLBACK_LINES: usize = 10_000;

/// User configuration loaded from `~/.comma.toml`. Every field is optional;
/// missing file, invalid TOML and unknown keys all fall back to defaults.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Terminal font size in points.
    pub font_size: f32,
    /// Initial window size in points.
    pub window_width: f32,
    pub window_height: f32,
    /// Width of the tab sidebar.
    pub sidebar_width: f32,
    /// Scrollback history in lines.
    pub scrollback_lines: usize,
    /// Shell to run in new tabs (see `pty::shell_path` for the priority).
    pub shell: Option<String>,
    /// Color overrides (`[colors]` section, `#RRGGBB` strings).
    pub colors: Colors,
}

/// Color overrides from the `[colors]` section. Kept as raw strings so one
/// invalid value doesn't invalidate the whole config; resolution (with
/// warnings) happens in `render::Palette::with_overrides`.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct Colors {
    pub foreground: Option<String>,
    pub background: Option<String>,
    pub cursor: Option<String>,
    pub selection_background: Option<String>,
    pub black: Option<String>,
    pub red: Option<String>,
    pub green: Option<String>,
    pub yellow: Option<String>,
    pub blue: Option<String>,
    pub magenta: Option<String>,
    pub cyan: Option<String>,
    pub white: Option<String>,
    pub bright_black: Option<String>,
    pub bright_red: Option<String>,
    pub bright_green: Option<String>,
    pub bright_yellow: Option<String>,
    pub bright_blue: Option<String>,
    pub bright_magenta: Option<String>,
    pub bright_cyan: Option<String>,
    pub bright_white: Option<String>,
}

/// Names of the 16 palette slots, in palette index order (must match
/// `PALETTE`).
pub const COLOR_NAMES: [&str; 16] = [
    "black", "red", "green", "yellow", "blue", "magenta", "cyan", "white",
    "bright_black", "bright_red", "bright_green", "bright_yellow", "bright_blue",
    "bright_magenta", "bright_cyan", "bright_white",
];

impl Colors {
    /// Override strings for the 16 indexed slots, in palette order.
    pub fn indexed(&self) -> [&Option<String>; 16] {
        [
            &self.black, &self.red, &self.green, &self.yellow, &self.blue, &self.magenta,
            &self.cyan, &self.white, &self.bright_black, &self.bright_red, &self.bright_green,
            &self.bright_yellow, &self.bright_blue, &self.bright_magenta, &self.bright_cyan,
            &self.bright_white,
        ]
    }
}

/// Parse a `#RRGGBB` color string.
pub fn parse_hex_color(text: &str) -> Option<Rgb> {
    let hex = text.strip_prefix('#')?;
    if hex.len() != 6 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let channel = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok();
    Some(Rgb { r: channel(0)?, g: channel(2)?, b: channel(4)? })
}

impl Default for Config {
    fn default() -> Self {
        Self {
            font_size: FONT_SIZE,
            window_width: WINDOW_WIDTH,
            window_height: WINDOW_HEIGHT,
            sidebar_width: SIDEBAR_WIDTH,
            scrollback_lines: SCROLLBACK_LINES,
            shell: None,
            colors: Colors::default(),
        }
    }
}

impl Config {
    /// Load `~/.comma.toml`; a missing file, read error or invalid TOML
    /// yields the defaults (with a warning for everything but NotFound).
    pub fn load() -> Self {
        let Some(path) =
            std::env::var_os("HOME").map(|home| std::path::PathBuf::from(home).join(".comma.toml"))
        else {
            return Self::default();
        };
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Self::default(),
            Err(err) => {
                eprintln!("comma: cannot read {}: {err}", path.display());
                return Self::default();
            }
        };
        Self::from_toml(&text).unwrap_or_else(|err| {
            eprintln!("comma: ignoring invalid {}: {err}", path.display());
            Self::default()
        })
    }

    /// Parse config text. Unknown keys are ignored, missing keys default.
    fn from_toml(text: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(text)
    }
}

/// Tab labels longer than this are truncated with an ellipsis.
pub const MAX_TAB_LABEL: usize = 24;

/// Grid size a tab is spawned with; the first frame resizes it.
pub const START_COLUMNS: usize = 80;
pub const START_LINES: usize = 24;

/// Default tab title before the shell sets one via escape sequences.
pub fn default_tab_title(id: usize) -> String {
    format!("shell {id}")
}

pub const DEFAULT_FG: Rgb = Rgb { r: 0xd8, g: 0xd8, b: 0xd8 };
pub const DEFAULT_BG: Rgb = Rgb { r: 0x18, g: 0x18, b: 0x18 };
pub const DEFAULT_CURSOR: Rgb = Rgb { r: 0xd8, g: 0xd8, b: 0xd8 };

/// Normal and bright ANSI colors (default terminal palette).
pub const PALETTE: [Rgb; 16] = [
    Rgb { r: 0x1d, g: 0x1f, b: 0x21 }, // black
    Rgb { r: 0xcc, g: 0x66, b: 0x66 }, // red
    Rgb { r: 0xb5, g: 0xbd, b: 0x68 }, // green
    Rgb { r: 0xf0, g: 0xc6, b: 0x74 }, // yellow
    Rgb { r: 0x81, g: 0xa2, b: 0xbe }, // blue
    Rgb { r: 0xb2, g: 0x94, b: 0xbb }, // magenta
    Rgb { r: 0x8a, g: 0xbe, b: 0xb7 }, // cyan
    Rgb { r: 0xc5, g: 0xc8, b: 0xc6 }, // white
    Rgb { r: 0x66, g: 0x66, b: 0x66 }, // bright black
    Rgb { r: 0xd5, g: 0x4e, b: 0x53 }, // bright red
    Rgb { r: 0xb9, g: 0xca, b: 0x4a }, // bright green
    Rgb { r: 0xe7, g: 0xc5, b: 0x47 }, // bright yellow
    Rgb { r: 0x7a, g: 0xa6, b: 0xda }, // bright blue
    Rgb { r: 0xc3, g: 0x97, b: 0xd8 }, // bright magenta
    Rgb { r: 0x70, g: 0xc0, b: 0xb1 }, // bright cyan
    Rgb { r: 0xea, g: 0xea, b: 0xea }, // bright white
];

/// Background color of selected cells.
pub const SELECTION_BG: Color32 = Color32::from_rgb(0x3d, 0x44, 0x52);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_config_parses() {
        let config = Config::from_toml(
            r#"
                font_size = 18.0
                window_width = 1200.0
                window_height = 800.0
                sidebar_width = 200.0
                scrollback_lines = 5000
                shell = "/bin/bash"
            "#,
        )
        .unwrap();
        assert_eq!(
            config,
            Config {
                font_size: 18.0,
                window_width: 1200.0,
                window_height: 800.0,
                sidebar_width: 200.0,
                scrollback_lines: 5000,
                shell: Some("/bin/bash".into()),
                colors: Colors::default(),
            }
        );
    }

    #[test]
    fn colors_parse_from_toml() {
        let config = Config::from_toml(
            r##"
                [colors]
                foreground = "#112233"
                red = "#cc6666"
                bright_cyan = "#70c0b1"
            "##,
        )
        .unwrap();
        assert_eq!(config.colors.foreground.as_deref(), Some("#112233"));
        assert_eq!(config.colors.red.as_deref(), Some("#cc6666"));
        assert_eq!(config.colors.bright_cyan.as_deref(), Some("#70c0b1"));
        assert_eq!(config.colors.blue, None);
        // No [colors] section: everything stays None (defaults downstream).
        assert_eq!(Config::from_toml("font_size = 14.0").unwrap().colors, Colors::default());
    }

    #[test]
    fn hex_color_parsing() {
        assert_eq!(parse_hex_color("#d8d8d8"), Some(Rgb { r: 0xd8, g: 0xd8, b: 0xd8 }));
        assert_eq!(parse_hex_color("#1D1f21"), Some(Rgb { r: 0x1d, g: 0x1f, b: 0x21 }));
        assert_eq!(parse_hex_color("d8d8d8"), None); // missing '#'
        assert_eq!(parse_hex_color("#xyzxyz"), None); // not hex
        assert_eq!(parse_hex_color("#123"), None); // too short
    }

    #[test]
    fn partial_config_defaults_the_rest() {
        let config = Config::from_toml("font_size = 20.0").unwrap();
        assert_eq!(config.font_size, 20.0);
        assert_eq!(
            config,
            Config { font_size: 20.0, ..Config::default() },
            "only font_size differs from defaults"
        );
    }

    #[test]
    fn broken_config_falls_back_to_defaults() {
        assert!(Config::from_toml("font_size = [broken").is_err());
        // Same fallback Config::load applies on a parse error.
        let config = Config::from_toml("font_size = [broken").unwrap_or_default();
        assert_eq!(config, Config::default());
    }

    #[test]
    fn unknown_fields_are_ignored() {
        let config = Config::from_toml("unknown_key = true\nfont_size = 16.0").unwrap();
        assert_eq!(config.font_size, 16.0);
        assert_eq!(config.window_width, WINDOW_WIDTH);
    }
}
