//! Colour theme: named style slots parsed from core `Config` strings.
//!
//! Mirrors the `[keys]` pattern exactly — `churl-core` stays TUI-free and carries
//! only strings (`theme` selects a built-in, `[theme_colors]` overrides slots by
//! name); this TUI-layer module parses them into ratatui [`Style`]s and **fails
//! loudly** at startup on an unknown slot name, bad colour, or unknown built-in.
//!
//! Colour values are named ANSI colours (`red`, `light-blue`, …) or `#rrggbb`
//! hex. The set of slots is data-driven (one table) so a new slot is one line.

use std::collections::BTreeMap;

use color_eyre::eyre::{Result, eyre};
use ratatui::style::{Color, Modifier, Style};

/// A resolved colour theme: one [`Style`] per named slot. Threaded through every
/// component render function (the App owns it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Theme {
    /// Border of the focused pane.
    pub border_focused: Style,
    /// Border of an unfocused pane.
    pub border_unfocused: Style,
    /// Pane titles.
    pub title: Style,
    /// Selected/cursor row in the explorer and pickers. Also the bright,
    /// filled background of the ACTIVE tab chip in both tab bars.
    pub selection: Style,
    /// The dim, filled background of an INACTIVE tab chip in both tab bars —
    /// a notch off the pane background so a chip reads as filled-but-dim,
    /// clearly distinct from the bright active `selection` chip.
    pub tab_inactive: Style,
    /// The status line.
    pub statusline: Style,
    /// An error/warning message in the status line.
    pub status_error: Style,
    /// A masked literal secret (`*****`) in the request pane.
    pub auth_mask: Style,
    /// A response status summary line.
    pub response_status: Style,
    /// A jump-mode label character.
    pub jump_label: Style,
    /// Emphasis accent for steady state markers (the unsaved `●` in the
    /// statusline / URL bar / explorer row).
    pub accent: Style,
}

impl Theme {
    /// The built-in dark theme (the default; keeps M5 rendering).
    pub fn dark() -> Self {
        Self {
            border_focused: Style::default().fg(Color::Cyan),
            border_unfocused: Style::default().fg(Color::DarkGray),
            title: Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
            selection: Style::default().fg(Color::Black).bg(Color::Cyan),
            // Dim filled chip: light-gray text on a dark-gray fill — a notch
            // off the black pane bg, clearly dimmer than the bright cyan active.
            tab_inactive: Style::default().fg(Color::Gray).bg(Color::DarkGray),
            statusline: Style::default().fg(Color::Gray),
            status_error: Style::default().fg(Color::Red),
            auth_mask: Style::default().fg(Color::DarkGray),
            response_status: Style::default().fg(Color::Green),
            jump_label: Style::default()
                .fg(Color::Black)
                .bg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
            accent: Style::default().fg(Color::Yellow),
        }
    }

    /// The built-in light theme.
    pub fn light() -> Self {
        Self {
            border_focused: Style::default().fg(Color::Blue),
            border_unfocused: Style::default().fg(Color::Gray),
            title: Style::default()
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD),
            selection: Style::default().fg(Color::White).bg(Color::Blue),
            // Dim filled chip: dark-gray text on a light-gray fill — a notch off
            // the white pane bg, clearly dimmer than the bright blue active.
            tab_inactive: Style::default().fg(Color::DarkGray).bg(Color::Gray),
            statusline: Style::default().fg(Color::DarkGray),
            status_error: Style::default().fg(Color::Red),
            auth_mask: Style::default().fg(Color::Gray),
            response_status: Style::default().fg(Color::Green),
            jump_label: Style::default()
                .fg(Color::White)
                .bg(Color::Magenta)
                .add_modifier(Modifier::BOLD),
            accent: Style::default().fg(Color::Magenta),
        }
    }

    /// Resolves a theme from core config: selects the built-in named by `name`
    /// (`None`/`"dark"` → dark, `"light"` → light; anything else is a hard error)
    /// and applies each `[theme_colors]` override by slot name.
    ///
    /// Unknown built-in names, unknown slot names, and unparseable colours are all
    /// startup errors naming the offending value — no silent fallback.
    pub fn resolve(name: Option<&str>, overrides: &BTreeMap<String, String>) -> Result<Self> {
        let mut theme = match name {
            None | Some("dark") => Self::dark(),
            Some("light") => Self::light(),
            Some(other) => {
                return Err(eyre!(
                    "unknown theme {other:?} (built-ins: \"dark\", \"light\")"
                ));
            }
        };
        for (slot, value) in overrides {
            let color = parse_color(value)
                .ok_or_else(|| eyre!("bad colour {value:?} for theme slot {slot:?}"))?;
            let target = theme
                .slot_mut(slot)
                .ok_or_else(|| eyre!("unknown theme slot {slot:?} in [theme_colors]"))?;
            *target = target.fg(color);
        }
        Ok(theme)
    }

    /// Returns a mutable reference to the [`Style`] for `slot`, or `None` when the
    /// name is not a known slot. The single mapping of slot name → field.
    fn slot_mut(&mut self, slot: &str) -> Option<&mut Style> {
        Some(match slot {
            "border_focused" => &mut self.border_focused,
            "border_unfocused" => &mut self.border_unfocused,
            "title" => &mut self.title,
            "selection" => &mut self.selection,
            "tab_inactive" => &mut self.tab_inactive,
            "statusline" => &mut self.statusline,
            "status_error" => &mut self.status_error,
            "auth_mask" => &mut self.auth_mask,
            "response_status" => &mut self.response_status,
            "jump_label" => &mut self.jump_label,
            "accent" => &mut self.accent,
            _ => return None,
        })
    }

    /// Whether this is the light built-in, so the highlight worker can match
    /// response bodies to the pane palette.
    pub fn is_light(&self) -> bool {
        *self == Self::light()
    }
}

impl Default for Theme {
    fn default() -> Self {
        Self::dark()
    }
}

/// Parses a colour value: a `#rrggbb` hex triple or a named ANSI colour.
/// Returns `None` on any parse failure (the caller turns that into a loud error).
fn parse_color(value: &str) -> Option<Color> {
    let value = value.trim();
    if let Some(hex) = value.strip_prefix('#') {
        if hex.len() != 6 {
            return None;
        }
        let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
        let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
        let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
        return Some(Color::Rgb(r, g, b));
    }
    match value.to_ascii_lowercase().as_str() {
        "black" => Some(Color::Black),
        "red" => Some(Color::Red),
        "green" => Some(Color::Green),
        "yellow" => Some(Color::Yellow),
        "blue" => Some(Color::Blue),
        "magenta" => Some(Color::Magenta),
        "cyan" => Some(Color::Cyan),
        "gray" | "grey" => Some(Color::Gray),
        "dark-gray" | "dark-grey" | "darkgray" => Some(Color::DarkGray),
        "light-red" => Some(Color::LightRed),
        "light-green" => Some(Color::LightGreen),
        "light-yellow" => Some(Color::LightYellow),
        "light-blue" => Some(Color::LightBlue),
        "light-magenta" => Some(Color::LightMagenta),
        "light-cyan" => Some(Color::LightCyan),
        "white" => Some(Color::White),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn overrides(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn selects_built_in_by_name() {
        assert_eq!(
            Theme::resolve(None, &BTreeMap::new()).unwrap(),
            Theme::dark()
        );
        assert_eq!(
            Theme::resolve(Some("dark"), &BTreeMap::new()).unwrap(),
            Theme::dark()
        );
        assert_eq!(
            Theme::resolve(Some("light"), &BTreeMap::new()).unwrap(),
            Theme::light()
        );
    }

    #[test]
    fn unknown_built_in_is_error() {
        let err = Theme::resolve(Some("gruvbox"), &BTreeMap::new()).unwrap_err();
        assert!(err.to_string().contains("gruvbox"), "{err}");
    }

    #[test]
    fn named_and_hex_overrides_apply() {
        let theme = Theme::resolve(
            None,
            &overrides(&[("title", "red"), ("selection", "#112233")]),
        )
        .unwrap();
        assert_eq!(theme.title.fg, Some(Color::Red));
        assert_eq!(theme.selection.fg, Some(Color::Rgb(0x11, 0x22, 0x33)));
        // An untouched slot keeps its built-in value.
        assert_eq!(theme.border_focused, Theme::dark().border_focused);
    }

    #[test]
    fn override_wins_over_built_in() {
        // The light built-in has a blue focused border; overriding to green wins.
        let theme =
            Theme::resolve(Some("light"), &overrides(&[("border_focused", "green")])).unwrap();
        assert_eq!(theme.border_focused.fg, Some(Color::Green));
    }

    #[test]
    fn unknown_slot_is_error() {
        let err = Theme::resolve(None, &overrides(&[("bogus_slot", "red")])).unwrap_err();
        assert!(err.to_string().contains("bogus_slot"), "{err}");
    }

    #[test]
    fn bad_hex_is_error() {
        for bad in ["#12", "#gggggg", "#1234567"] {
            let err = Theme::resolve(None, &overrides(&[("title", bad)])).unwrap_err();
            assert!(err.to_string().contains("title"), "{err} for {bad}");
        }
    }

    #[test]
    fn bad_named_colour_is_error() {
        let err = Theme::resolve(None, &overrides(&[("title", "turquoise")])).unwrap_err();
        assert!(err.to_string().contains("turquoise"), "{err}");
    }
}
