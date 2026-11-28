//! CPZed: badge chips, the right-side status glyph and the child-count chip of a
//! terminal thread row. All colors come from theme tokens (never hex); every
//! chip keeps its text so nothing is conveyed by color alone.

use agent_ui::terminal_attention::SessionState;
use gpui::{AnyElement, App, Hsla, IntoElement, ParentElement, SharedString, Styled, div, px};
use theme::{ActiveTheme as _, StatusColors, ThemeColors};
use ui::{Tooltip, prelude::*};

/// Same cap as `cli::MAX_BADGES` (the CLI validates; this guards a hand-edited store).
pub(crate) const MAX_CHIPS: usize = 4;

/// What a chip's text means (drives its color).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ChipKind {
    Bg,
    Fg,
    Opus,
    Sonnet,
    Fable,
    Haiku,
    Rc,
    Other,
}

impl ChipKind {
    pub(crate) fn of(text: &str) -> Self {
        match text {
            "bg" => Self::Bg,
            "fg" => Self::Fg,
            "opus" => Self::Opus,
            "sonnet" => Self::Sonnet,
            "fable" => Self::Fable,
            "haiku" => Self::Haiku,
            "rc" => Self::Rc,
            _ => Self::Other,
        }
    }
}

/// Text/border color of a chip kind, from theme tokens.
pub(crate) fn chip_accent(kind: ChipKind, colors: &ThemeColors) -> Hsla {
    match kind {
        ChipKind::Bg | ChipKind::Other => colors.text_muted,
        ChipKind::Fg => colors.text_accent,
        ChipKind::Opus => colors.terminal_ansi_magenta,
        ChipKind::Sonnet => colors.terminal_ansi_blue,
        ChipKind::Fable => colors.terminal_ansi_cyan,
        ChipKind::Haiku => colors.terminal_ansi_green,
        ChipKind::Rc => colors.terminal_ansi_yellow,
    }
}

/// Fill of a chip: the accent tinted at 15% over the panel background.
pub(crate) fn chip_fill(kind: ChipKind, colors: &ThemeColors) -> Hsla {
    match kind {
        ChipKind::Bg | ChipKind::Other => colors.element_background,
        _ => chip_accent(kind, colors).opacity(0.15),
    }
}

/// Status color of a session state: working = info, blocked = warning,
/// done = success, anything else muted.
pub(crate) fn state_color(
    state: Option<SessionState>,
    status: &StatusColors,
    colors: &ThemeColors,
) -> Hsla {
    match state {
        Some(SessionState::Working) => status.info,
        Some(SessionState::Blocked) => status.warning,
        Some(SessionState::Done) => status.success,
        _ => colors.text_muted,
    }
}

#[cfg(test)]
fn channel(value: f32) -> f32 {
    if value <= 0.03928 {
        value / 12.92
    } else {
        ((value + 0.055) / 1.055).powf(2.4)
    }
}

#[cfg(test)]
fn luminance(color: Hsla) -> f32 {
    let rgb = color.to_rgb();
    0.2126 * channel(rgb.r) + 0.7152 * channel(rgb.g) + 0.0722 * channel(rgb.b)
}

/// WCAG contrast ratio of `fg` over `bg` (alpha of `bg` composited on `base`).
#[cfg(test)]
pub(crate) fn contrast(fg: Hsla, bg: Hsla, base: Hsla) -> f32 {
    let bg = base.blend(bg);
    let (a, b) = (luminance(fg), luminance(bg));
    let (hi, lo) = if a > b { (a, b) } else { (b, a) };
    (hi + 0.05) / (lo + 0.05)
}

/// The text of the chips to draw: validated badge strings, in order.
pub(crate) fn visible_badges(badges: &[String]) -> Vec<String> {
    badges
        .iter()
        .filter(|badge| !badge.is_empty())
        .take(MAX_CHIPS)
        .cloned()
        .collect()
}

/// Tooltip of the child-count chip.
pub(crate) fn child_tooltip(count: usize) -> String {
    format!("{count} child thread{}", if count == 1 { "" } else { "s" })
}

fn chip(text: SharedString, kind: ChipKind, cx: &App) -> AnyElement {
    let colors = cx.theme().colors();
    let accent = chip_accent(kind, colors);
    div()
        .px_1()
        .rounded_sm()
        .border_1()
        .border_color(accent.opacity(0.6))
        .bg(chip_fill(kind, colors))
        .child(
            Label::new(text)
                .size(LabelSize::XSmall)
                .color(Color::Custom(accent)),
        )
        .into_any_element()
}

/// Solid square-cornered chip: element_selected fill, border, text.
fn child_count_chip(count: usize, cx: &App) -> AnyElement {
    let colors = cx.theme().colors();
    div()
        .id("child-count")
        .px_1()
        .border_1()
        .border_color(colors.border)
        .bg(colors.element_selected)
        .child(
            Label::new(count.to_string())
                .size(LabelSize::XSmall)
                .color(Color::Default),
        )
        .tooltip(Tooltip::text(child_tooltip(count)))
        .into_any_element()
}

/// Text and fill of the red pending badge.
pub(crate) fn pending_colors(colors: &ThemeColors, status: &StatusColors) -> (Hsla, Hsla) {
    (colors.editor_foreground, status.error)
}

/// Red numeric badge (hidden at 0).
pub(crate) fn pending_badge(count: usize, cx: &App) -> Option<AnyElement> {
    if count == 0 {
        return None;
    }
    let (text, fill) = pending_colors(cx.theme().colors(), cx.theme().status());
    Some(
        div()
            .id("pending-input")
            .px_1p5()
            .rounded_full()
            .min_w(px(16.))
            .bg(fill)
            .child(
                Label::new(count.to_string())
                    .size(LabelSize::XSmall)
                    .color(Color::Custom(text)),
            )
            .tooltip(Tooltip::text("waiting for your input"))
            .into_any_element(),
    )
}

/// Sum of blocked sessions over every attach thread: the sidebar header number.
pub(crate) fn header_total(cx: &App) -> usize {
    agent_ui::terminal_attention::TerminalAttention::try_global(cx)
        .map_or(0, |attention| attention.read(cx).total_pending())
}

/// The sidebar header's red badge: [`header_total`], hidden at 0.
pub(crate) fn header_pending(cx: &App) -> Option<AnyElement> {
    pending_badge(header_total(cx), cx).map(|badge| {
        div()
            .debug_selector(|| "sidebar-header-pending".into())
            .child(badge)
            .into_any_element()
    })
}

pub(crate) struct TrailingRow<'a> {
    pub badges: &'a [String],
    pub pending: usize,
    pub child_count: usize,
    pub glyph: Option<SharedString>,
    pub state: Option<SessionState>,
}

/// One piece of the trailing area, left to right.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Part {
    Chip(String),
    ChildCount(usize),
    Pending(usize),
    /// The status glyph: always last, i.e. at the right edge of the tile.
    Glyph(SharedString),
}

impl TrailingRow<'_> {
    pub(crate) fn parts(&self) -> Vec<Part> {
        let mut parts: Vec<Part> = visible_badges(self.badges)
            .into_iter()
            .map(Part::Chip)
            .collect();
        if self.child_count > 0 {
            parts.push(Part::ChildCount(self.child_count));
        }
        if self.pending > 0 {
            parts.push(Part::Pending(self.pending));
        }
        if let Some(glyph) = &self.glyph {
            parts.push(Part::Glyph(glyph.clone()));
        }
        parts
    }
}

/// The trailing element of a terminal row, or `None` when there is nothing.
pub(crate) fn render_trailing(row: &TrailingRow<'_>, cx: &App) -> Option<AnyElement> {
    let parts = row.parts();
    if parts.is_empty() {
        return None;
    }
    let colors = cx.theme().colors();
    let status = cx.theme().status();
    Some(
        h_flex()
            .gap_1()
            .children(parts.into_iter().filter_map(|part| {
                match part {
                    Part::Chip(text) => Some(chip(text.clone().into(), ChipKind::of(&text), cx)),
                    Part::ChildCount(count) => Some(child_count_chip(count, cx)),
                    Part::Pending(count) => pending_badge(count, cx),
                    Part::Glyph(glyph) => Some(
                        Label::new(glyph)
                            .size(LabelSize::Small)
                            .color(Color::Custom(state_color(row.state, status, colors)))
                            .into_any_element(),
                    ),
                }
            }))
            .into_any_element(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use theme::{StatusColors, ThemeColors};

    fn themes() -> Vec<(&'static str, ThemeColors, StatusColors)> {
        vec![
            ("light", ThemeColors::light(), StatusColors::light()),
            ("dark", ThemeColors::dark(), StatusColors::dark()),
        ]
    }

    #[test]
    fn kinds_map_from_text() {
        assert_eq!(ChipKind::of("bg"), ChipKind::Bg);
        assert_eq!(ChipKind::of("rc"), ChipKind::Rc);
        assert_eq!(ChipKind::of("opus"), ChipKind::Opus);
        assert_eq!(ChipKind::of("whatever"), ChipKind::Other);
        assert_eq!(ChipKind::of("Opus"), ChipKind::Other);
    }

    #[test]
    fn model_chips_differ_in_both_themes() {
        for (_name, colors, _) in themes() {
            let colors = &colors;
            let all = [
                ChipKind::Opus,
                ChipKind::Sonnet,
                ChipKind::Fable,
                ChipKind::Haiku,
            ];
            for (i, a) in all.iter().enumerate() {
                for b in &all[i + 1..] {
                    assert_ne!(
                        chip_accent(*a, colors),
                        chip_accent(*b, colors),
                        "{a:?} vs {b:?}"
                    );
                }
            }
            assert_ne!(
                chip_accent(ChipKind::Bg, colors),
                chip_accent(ChipKind::Fg, colors)
            );
        }
    }

    #[test]
    fn chip_text_is_readable_in_light_and_dark() {
        for (name, colors, status) in themes() {
            let colors = &colors;
            for kind in [
                ChipKind::Bg,
                ChipKind::Fg,
                ChipKind::Opus,
                ChipKind::Sonnet,
                ChipKind::Fable,
                ChipKind::Haiku,
                ChipKind::Rc,
                ChipKind::Other,
            ] {
                let ratio = contrast(
                    chip_accent(kind, colors),
                    chip_fill(kind, colors),
                    colors.panel_background,
                );
                assert!(ratio >= 3.0, "{name} {kind:?} contrast {ratio}");
            }
            // Red pending badge: its number over status.error.
            let (text, fill) = pending_colors(colors, &status);
            let ratio = contrast(text, fill, colors.panel_background);
            assert!(ratio >= 3.0, "{name} pending badge contrast {ratio}");
            // Child-count chip: text over element_selected.
            let ratio = contrast(
                colors.text,
                colors.element_selected,
                colors.panel_background,
            );
            assert!(ratio >= 3.0, "{name} child count contrast {ratio}");
        }
    }

    #[test]
    fn state_colors_follow_the_status_mapping() {
        for (_, colors, status) in themes() {
            let (colors, status) = (&colors, &status);
            assert_eq!(
                state_color(Some(SessionState::Working), status, colors),
                status.info
            );
            assert_eq!(
                state_color(Some(SessionState::Blocked), status, colors),
                status.warning
            );
            assert_eq!(
                state_color(Some(SessionState::Done), status, colors),
                status.success
            );
            assert_eq!(
                state_color(Some(SessionState::Other), status, colors),
                colors.text_muted
            );
            assert_eq!(state_color(None, status, colors), colors.text_muted);
        }
    }

    #[test]
    fn badges_are_capped_and_blanks_dropped() {
        let many: Vec<String> = ["a", "", "b", "c", "d", "e"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(visible_badges(&many), ["a", "b", "c", "d"]);
        assert!(visible_badges(&[]).is_empty());
    }

    #[test]
    fn child_tooltip_pluralizes() {
        assert_eq!(child_tooltip(1), "1 child thread");
        assert_eq!(child_tooltip(3), "3 child threads");
    }

    #[test]
    fn contrast_is_symmetric_and_bounded() {
        let black = gpui::black();
        let white = gpui::white();
        let r = contrast(black, white, white);
        assert!(r > 20.0 && r <= 21.1);
        assert!((contrast(white, black, white) - r).abs() < 0.01);
    }

    fn row<'a>(
        badges: &'a [String],
        child_count: usize,
        pending: usize,
        glyph: Option<&str>,
    ) -> TrailingRow<'a> {
        TrailingRow {
            badges,
            pending,
            child_count,
            glyph: glyph.map(|g| g.to_string().into()),
            state: None,
        }
    }

    #[test]
    fn glyph_is_the_rightmost_part() {
        let badges = vec!["bg".to_string(), "opus".to_string()];
        let parts = row(&badges, 2, 1, Some("✳")).parts();
        assert_eq!(
            parts,
            [
                Part::Chip("bg".into()),
                Part::Chip("opus".into()),
                Part::ChildCount(2),
                Part::Pending(1),
                Part::Glyph("✳".into()),
            ]
        );
        assert!(matches!(parts.last(), Some(Part::Glyph(_))));
    }

    #[test]
    fn child_count_is_hidden_at_zero_and_exact_for_one_and_many() {
        for (count, expected) in [(0, false), (1, true), (7, true)] {
            let parts = row(&[], count, 0, None).parts();
            assert_eq!(
                parts.contains(&Part::ChildCount(count)),
                expected,
                "{count}"
            );
            assert_eq!(parts.is_empty(), !expected);
        }
    }

    #[test]
    fn pending_is_hidden_at_zero_and_nothing_means_no_parts() {
        assert!(row(&[], 0, 0, None).parts().is_empty());
        assert_eq!(row(&[], 0, 3, None).parts(), [Part::Pending(3)]);
    }

    #[test]
    fn worst_case_trailing_row_is_bounded_and_keeps_every_part() {
        // The title area truncates before the trailing row (the row is
        // `flex_none` and the fades never cover it), so the widest row must
        // stay bounded: 4 chips + child count + pending + glyph.
        let badges: Vec<String> = (0..9).map(|i| format!("chip{i}-12chars")).collect();
        let parts = row(&badges, 3, 2, Some("✳")).parts();
        assert_eq!(parts.len(), 7);
        assert_eq!(
            parts
                .iter()
                .filter(|part| matches!(part, Part::Chip(_)))
                .count(),
            4
        );
        assert!(matches!(parts.last(), Some(Part::Glyph(_))));
    }
}
