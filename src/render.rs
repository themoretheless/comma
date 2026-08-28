//! Rendering of the terminal grid with an `egui::Painter`.

use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::color::Colors;
use alacritty_terminal::term::{Term, TermDamage};
use alacritty_terminal::vte::ansi::{Color, CursorShape, NamedColor, Rgb};
use egui::text::{ByteIndex, LayoutJob, LayoutSection, TextFormat};
use egui::{Align2, Color32, FontId, Painter, Pos2, Rect, Stroke, Vec2};

use crate::config;
use crate::input;
use crate::pty::EventProxy;

/// (width, height) of a terminal cell in points.
pub(crate) fn cell_size(ctx: &egui::Context, font_size: f32) -> (f32, f32) {
    let font = FontId::monospace(font_size);
    ctx.fonts_mut(|fonts| (fonts.glyph_width(&font, 'M'), fonts.row_height(&font)))
}

fn dim(rgb: Rgb) -> Rgb {
    Rgb {
        r: (rgb.r as f32 * 0.66) as u8,
        g: (rgb.g as f32 * 0.66) as u8,
        b: (rgb.b as f32 * 0.66) as u8,
    }
}

fn to_color32(rgb: Rgb) -> Color32 {
    Color32::from_rgb(rgb.r, rgb.g, rgb.b)
}

/// Resolved color set used for drawing; defaults are the built-in constants
/// from `config`, overridable via `[colors]` in `~/.comma.toml`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Palette {
    pub foreground: Rgb,
    pub background: Rgb,
    pub cursor: Rgb,
    pub selection: Color32,
    /// Normal and bright ANSI colors (indices 0..16).
    pub indexed: [Rgb; 16],
}

impl Default for Palette {
    fn default() -> Self {
        Self {
            foreground: config::DEFAULT_FG,
            background: config::DEFAULT_BG,
            cursor: config::DEFAULT_CURSOR,
            selection: config::SELECTION_BG,
            indexed: config::PALETTE,
        }
    }
}

impl Palette {
    /// Default palette with `[colors]` overrides applied; invalid hex values
    /// warn and keep the default.
    pub(crate) fn with_overrides(colors: &config::Colors) -> Self {
        let mut palette = Self::default();
        let apply = |slot: &mut Rgb, name: &str, value: &Option<String>| {
            if let Some(value) = value {
                match config::parse_hex_color(value) {
                    Some(rgb) => *slot = rgb,
                    None => eprintln!("comma: invalid color {name} = {value:?}, expected #RRGGBB"),
                }
            }
        };
        apply(&mut palette.foreground, "foreground", &colors.foreground);
        apply(&mut palette.background, "background", &colors.background);
        apply(&mut palette.cursor, "cursor", &colors.cursor);
        if let Some(value) = &colors.selection_background {
            match config::parse_hex_color(value) {
                Some(rgb) => palette.selection = to_color32(rgb),
                None => eprintln!(
                    "comma: invalid color selection_background = {value:?}, expected #RRGGBB"
                ),
            }
        }
        for (slot, (name, value)) in
            palette.indexed.iter_mut().zip(config::COLOR_NAMES.iter().zip(colors.indexed()))
        {
            apply(slot, name, value);
        }
        palette
    }
}

/// Resolve a terminal color to an RGB value, honoring palette overrides
/// set by the application through escape sequences.
fn resolve(color: Color, flags: Flags, colors: &Colors, is_fg: bool, palette: &Palette) -> Rgb {
    // Bold text on a base ANSI color is rendered with the bright variant.
    let color = match color {
        Color::Named(named) if is_fg && flags.contains(Flags::BOLD) => match named {
            NamedColor::Black => Color::Named(NamedColor::BrightBlack),
            NamedColor::Red => Color::Named(NamedColor::BrightRed),
            NamedColor::Green => Color::Named(NamedColor::BrightGreen),
            NamedColor::Yellow => Color::Named(NamedColor::BrightYellow),
            NamedColor::Blue => Color::Named(NamedColor::BrightBlue),
            NamedColor::Magenta => Color::Named(NamedColor::BrightMagenta),
            NamedColor::Cyan => Color::Named(NamedColor::BrightCyan),
            NamedColor::White => Color::Named(NamedColor::BrightWhite),
            _ => color,
        },
        _ => color,
    };

    let rgb = match color {
        Color::Spec(rgb) => rgb,
        Color::Named(named) => match colors[named] {
            Some(rgb) => rgb,
            None => named_default(named, palette),
        },
        Color::Indexed(index) => {
            match colors[index as usize] {
                Some(rgb) => rgb,
                None if index < 16 => palette.indexed[index as usize],
                None if index < 232 => {
                    let i = index - 16;
                    let channel = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
                    Rgb { r: channel(i / 36), g: channel(i / 6 % 6), b: channel(i % 6) }
                }
                None => {
                    let v = 8 + (index - 232) * 10;
                    Rgb { r: v, g: v, b: v }
                }
            }
        }
    };

    if flags.contains(Flags::DIM) { dim(rgb) } else { rgb }
}

fn named_default(named: NamedColor, palette: &Palette) -> Rgb {
    match named {
        NamedColor::Foreground => palette.foreground,
        NamedColor::Background => palette.background,
        NamedColor::Cursor => palette.cursor,
        NamedColor::DimBlack => dim(palette.indexed[0]),
        NamedColor::DimRed => dim(palette.indexed[1]),
        NamedColor::DimGreen => dim(palette.indexed[2]),
        NamedColor::DimYellow => dim(palette.indexed[3]),
        NamedColor::DimBlue => dim(palette.indexed[4]),
        NamedColor::DimMagenta => dim(palette.indexed[5]),
        NamedColor::DimCyan => dim(palette.indexed[6]),
        NamedColor::DimWhite => dim(palette.indexed[7]),
        named if (named as usize) < 16 => palette.indexed[named as usize],
        _ => palette.foreground,
    }
}

/// Text style of one cell; adjacent cells with equal styles share a section.
#[derive(Clone, Copy, PartialEq)]
struct CellStyle {
    fg: Color32,
    underline: bool,
    strikethrough: bool,
}

impl CellStyle {
    fn format(self, font: &FontId) -> TextFormat {
        let stroke = |on: bool| if on { Stroke::new(1.0, self.fg) } else { Stroke::NONE };
        TextFormat {
            font_id: font.clone(),
            color: self.fg,
            underline: stroke(self.underline),
            strikethrough: stroke(self.strikethrough),
            ..Default::default()
        }
    }
}

/// Text and style of one grid cell.
struct RowCell {
    text: String,
    style: CellStyle,
}

/// Build one [`LayoutJob`] for a grid row: the plain text of the row with one
/// section per run of equal style. Trailing blank cells are dropped; the row
/// starts at column 0, so glyphs stay on the grid. Returns `None` for a row
/// without any text.
fn layout_row(font: &FontId, cells: &[RowCell]) -> Option<LayoutJob> {
    let last = cells.iter().rposition(|cell| cell.text != " ")?;
    let mut job = LayoutJob::default();
    let mut style: Option<CellStyle> = None;
    for cell in &cells[..=last] {
        let start = job.text.len();
        job.text.push_str(&cell.text);
        match style {
            Some(prev) if prev == cell.style => {
                if let Some(section) = job.sections.last_mut() {
                    section.byte_range.end = ByteIndex(job.text.len());
                }
            }
            _ => {
                job.sections.push(LayoutSection {
                    leading_space: 0.0,
                    byte_range: ByteIndex(start)..ByteIndex(job.text.len()),
                    format: cell.style.format(font),
                });
                style = Some(cell.style);
            }
        }
    }
    Some(job)
}

/// Underline the cells covered by typed `http(s)://` URLs in a row (the
/// terminal's own detection; OSC-8 hyperlinks are underlined per cell).
/// Runs on row rebuild only, i.e. for damaged rows.
fn underline_urls(cells: &mut [RowCell]) {
    let text: String = cells.iter().map(|cell| cell.text.as_str()).collect();
    let spans = input::find_urls(&text);
    if spans.is_empty() {
        return;
    }
    let mut offset = 0; // char index of the current cell in the row text
    for cell in cells.iter_mut() {
        let len = cell.text.chars().count();
        if spans.iter().any(|&(start, end)| offset < end && start < offset + len) {
            cell.style.underline = true;
        }
        offset += len;
    }
}

/// Per-tab cache of shaped text rows, so frames without terminal damage
/// don't rebuild `LayoutJob`s. Rows are `Arc<Galley>`s indexed by display
/// row; background rects and the cursor are repainted from the grid every
/// frame (they're cheap and the selection overlay must stay live).
///
/// Note: alacritty 0.26 has no `TermDamage::None` — an idle frame reports
/// `Partial` with just the cursor line(s), so the steady-state cost is one
/// rebuilt row per frame.
pub(crate) struct RowCache {
    rows: Vec<Option<std::sync::Arc<egui::Galley>>>,
    display_offset: usize,
    font_size: f32,
    screen_lines: usize,
}

impl RowCache {
    pub(crate) fn new() -> Self {
        Self { rows: Vec::new(), display_offset: 0, font_size: 0.0, screen_lines: 0 }
    }
}

/// What `Term::damage` reported, reduced to display-row indices.
enum Damage {
    Full,
    Partial(Vec<usize>),
}

/// Which cached rows to rebuild (`None` = all): everything on full damage
/// or when the cache context (scroll offset, font, viewport height) no
/// longer matches; just the damaged lines otherwise.
fn stale_rows(
    damage: &Damage,
    cache: &RowCache,
    display_offset: usize,
    font_size: f32,
    screen_lines: usize,
) -> Option<Vec<usize>> {
    if cache.display_offset != display_offset
        || cache.font_size != font_size
        || cache.screen_lines != screen_lines
    {
        return None;
    }
    match damage {
        Damage::Full => None,
        Damage::Partial(lines) => {
            Some(lines.iter().copied().filter(|line| *line < screen_lines).collect())
        }
    }
}

/// Draw the visible terminal content into `rect`.
///
/// `blink_visible` is the current blink phase (block cursor only);
/// `focused` = the window has keyboard focus (else the cursor is hollow).
#[allow(clippy::too_many_arguments)]
pub(crate) fn draw(
    painter: &Painter,
    term: &mut Term<EventProxy>,
    rect: Rect,
    cell: Vec2,
    font_size: f32,
    cache: &mut RowCache,
    palette: &Palette,
    blink_visible: bool,
    focused: bool,
) {
    let painter = painter.with_clip_rect(rect);
    let screen_lines = term.screen_lines();
    let display_offset = term.grid().display_offset();
    let damage = match term.damage() {
        TermDamage::Full => Damage::Full,
        TermDamage::Partial(lines) => Damage::Partial(lines.map(|line| line.line).collect()),
    };
    term.reset_damage();

    let stale = stale_rows(&damage, cache, display_offset, font_size, screen_lines);
    if stale.is_none() {
        cache.rows.clear();
        cache.rows.resize_with(screen_lines, || None);
    } else if let Some(lines) = &stale {
        // A damaged row may have become blank: clear it before re-collecting.
        for &row in lines {
            if let Some(entry) = cache.rows.get_mut(row) {
                *entry = None;
            }
        }
    }
    cache.display_offset = display_offset;
    cache.font_size = font_size;
    cache.screen_lines = screen_lines;

    let content = term.renderable_content();
    let colors = content.colors;
    let display_offset_i32 = display_offset as i32;
    let cursor = content.cursor;
    let selection = content.selection;
    let font = FontId::monospace(font_size);
    let default_bg = to_color32(resolve(
        Color::Named(NamedColor::Background),
        Flags::empty(),
        colors,
        false,
        palette,
    ));
    let default_fg = to_color32(resolve(
        Color::Named(NamedColor::Foreground),
        Flags::empty(),
        colors,
        true,
        palette,
    ));

    painter.rect_filled(rect, 0.0, default_bg);

    let is_stale = |row: usize| stale.as_ref().is_none_or(|lines| lines.contains(&row));
    // Text cells of stale rows, for the LayoutJob rebuild below.
    let mut stale_cells: std::collections::HashMap<usize, Vec<RowCell>> =
        std::collections::HashMap::new();
    let mut cursor_glyph: Option<char> = None;

    for indexed in content.display_iter {
        let tcell = &indexed.cell;
        if tcell.flags.contains(Flags::WIDE_CHAR_SPACER) {
            continue;
        }
        let row = indexed.point.line.0 + display_offset_i32;
        if row < 0 {
            continue;
        }
        let row = row as usize;
        let col = indexed.point.column.0;

        let mut fg = resolve(tcell.fg, tcell.flags, colors, true, palette);
        let mut bg = resolve(tcell.bg, tcell.flags, colors, false, palette);
        if tcell.flags.contains(Flags::INVERSE) {
            std::mem::swap(&mut fg, &mut bg);
        }
        if tcell.flags.contains(Flags::HIDDEN) {
            fg = bg;
        }

        let selected = selection.is_some_and(|s| s.contains(indexed.point));
        let bg32 = to_color32(bg);
        if selected || bg32 != default_bg {
            let bg32 = if selected { palette.selection } else { bg32 };
            // A wide char owns two cells: cover both halves with its background.
            let width = if tcell.flags.contains(Flags::WIDE_CHAR) { cell.x * 2.0 } else { cell.x };
            let pos = Pos2::new(rect.left() + col as f32 * cell.x, rect.top() + row as f32 * cell.y);
            painter.rect_filled(Rect::from_min_size(pos, Vec2::new(width, cell.y)), 0.0, bg32);
        }

        if indexed.point == cursor.point {
            cursor_glyph = Some(tcell.c);
        }
        if is_stale(row) {
            let mut text = tcell.c.to_string();
            if let Some(zerowidth) = tcell.zerowidth() {
                text.extend(zerowidth.iter());
            }
            let style = CellStyle {
                fg: to_color32(fg),
                // OSC-8 hyperlinks are underlined like terminal-detected URLs.
                underline: tcell.flags.intersects(Flags::ALL_UNDERLINES)
                    || tcell.hyperlink().is_some(),
                strikethrough: tcell.flags.contains(Flags::STRIKEOUT),
            };
            stale_cells.entry(row).or_default().push(RowCell { text, style });
        }
    }

    for (row, mut cells) in stale_cells {
        underline_urls(&mut cells);
        if let Some(entry) = cache.rows.get_mut(row) {
            *entry = layout_row(&font, &cells).map(|job| painter.layout_job(job));
        }
    }

    for (row, entry) in cache.rows.iter().enumerate() {
        if let Some(galley) = entry {
            let pos = Pos2::new(rect.left(), rect.top() + row as f32 * cell.y);
            painter.galley(pos, galley.clone(), default_fg);
        }
    }

    draw_cursor(
        &painter,
        cursor,
        colors,
        rect,
        cell,
        &font,
        default_bg,
        display_offset_i32,
        cursor_glyph,
        palette,
        blink_visible,
        focused,
    );
}

#[allow(clippy::too_many_arguments)]
fn draw_cursor(
    painter: &Painter,
    cursor: alacritty_terminal::term::RenderableCursor,
    colors: &Colors,
    rect: Rect,
    cell: Vec2,
    font: &FontId,
    default_bg: Color32,
    display_offset: i32,
    cursor_glyph: Option<char>,
    palette: &Palette,
    blink_visible: bool,
    focused: bool,
) {
    if display_offset != 0 {
        return;
    }
    let cursor_color = to_color32(match colors[NamedColor::Cursor] {
        Some(rgb) => rgb,
        None => palette.cursor,
    });
    let pos = Pos2::new(
        rect.left() + cursor.point.column.0 as f32 * cell.x,
        rect.top() + cursor.point.line.0 as f32 * cell.y,
    );
    let cursor_rect = Rect::from_min_size(pos, cell);

    // An unfocused window always shows a hollow cursor; a block cursor in
    // the hidden blink phase shows nothing (the text is still drawn).
    let shape = if focused { cursor.shape } else { CursorShape::HollowBlock };
    match shape {
        CursorShape::Hidden => {}
        CursorShape::Block if !blink_visible => {}
        CursorShape::Block => {
            painter.rect_filled(cursor_rect, 0.0, cursor_color);
            // Redraw the glyph under the cursor in the background color.
            if let Some(c) = cursor_glyph.filter(|c| *c != ' ') {
                painter.text(pos, Align2::LEFT_TOP, c.to_string(), font.clone(), default_bg);
            }
        }
        CursorShape::Underline => {
            let bar = Rect::from_min_size(
                Pos2::new(pos.x, pos.y + cell.y - 2.0),
                Vec2::new(cell.x, 2.0),
            );
            painter.rect_filled(bar, 0.0, cursor_color);
        }
        CursorShape::Beam => {
            let bar = Rect::from_min_size(pos, Vec2::new(2.0, cell.y));
            painter.rect_filled(bar, 0.0, cursor_color);
        }
        CursorShape::HollowBlock => {
            painter.rect_stroke(cursor_rect, 0.0, Stroke::new(1.0, cursor_color), egui::StrokeKind::Inside);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rgb(r: u8, g: u8, b: u8) -> Rgb {
        Rgb { r, g, b }
    }

    fn resolved(color: Color, flags: Flags) -> Rgb {
        resolve(color, flags, &Colors::default(), true, &Palette::default())
    }

    #[test]
    fn indexed_colors() {
        // 0..16: the ANSI palette.
        assert_eq!(resolved(Color::Indexed(0), Flags::empty()), config::PALETTE[0]);
        assert_eq!(resolved(Color::Indexed(15), Flags::empty()), config::PALETTE[15]);
        // 16..232: the 6x6x6 color cube.
        assert_eq!(resolved(Color::Indexed(16), Flags::empty()), rgb(0, 0, 0));
        assert_eq!(resolved(Color::Indexed(231), Flags::empty()), rgb(255, 255, 255));
        assert_eq!(resolved(Color::Indexed(21), Flags::empty()), rgb(0, 0, 255));
        // 232..256: the grayscale ramp.
        assert_eq!(resolved(Color::Indexed(232), Flags::empty()), rgb(8, 8, 8));
        assert_eq!(resolved(Color::Indexed(255), Flags::empty()), rgb(238, 238, 238));
    }

    #[test]
    fn named_colors() {
        assert_eq!(resolved(Color::Named(NamedColor::Red), Flags::empty()), config::PALETTE[1]);
        assert_eq!(
            resolved(Color::Named(NamedColor::BrightRed), Flags::empty()),
            config::PALETTE[9]
        );
        assert_eq!(
            resolved(Color::Named(NamedColor::Foreground), Flags::empty()),
            config::DEFAULT_FG
        );
        assert_eq!(
            resolved(Color::Named(NamedColor::Background), Flags::empty()),
            config::DEFAULT_BG
        );
        assert_eq!(resolved(Color::Named(NamedColor::DimRed), Flags::empty()), dim(config::PALETTE[1]));
    }

    #[test]
    fn spec_colors_pass_through() {
        let color = rgb(1, 2, 3);
        assert_eq!(resolved(Color::Spec(color), Flags::empty()), color);
    }

    #[test]
    fn bold_text_uses_bright_variants() {
        assert_eq!(
            resolved(Color::Named(NamedColor::Red), Flags::BOLD),
            config::PALETTE[9]
        );
        // Only for foreground, and only for the eight base colors.
        assert_eq!(
            resolve(Color::Named(NamedColor::Red), Flags::BOLD, &Colors::default(), false, &Palette::default()),
            config::PALETTE[1]
        );
        assert_eq!(
            resolved(Color::Named(NamedColor::BrightRed), Flags::BOLD),
            config::PALETTE[9]
        );
    }

    #[test]
    fn dim_scales_channels() {
        assert_eq!(resolved(Color::Indexed(231), Flags::DIM), dim(rgb(255, 255, 255)));
        assert_eq!(dim(rgb(255, 255, 255)), rgb(168, 168, 168));
    }

    #[test]
    fn palette_overrides_win() {
        let mut colors = Colors::default();
        colors[NamedColor::Red] = Some(rgb(10, 20, 30));
        assert_eq!(
            resolve(Color::Named(NamedColor::Red), Flags::empty(), &colors, true, &Palette::default()),
            rgb(10, 20, 30)
        );
    }

    #[test]
    fn palette_with_config_overrides() {
        let colors = config::Colors {
            foreground: Some("#112233".into()),
            red: Some("#010203".into()),
            selection_background: Some("#040506".into()),
            ..Default::default()
        };
        let palette = Palette::with_overrides(&colors);
        assert_eq!(palette.foreground, rgb(0x11, 0x22, 0x33));
        assert_eq!(palette.indexed[1], rgb(1, 2, 3));
        assert_eq!(palette.selection, Color32::from_rgb(4, 5, 6));
        // Untouched slots keep the defaults.
        assert_eq!(palette.background, config::DEFAULT_BG);
        assert_eq!(palette.indexed[2], config::PALETTE[2]);
    }

    #[test]
    fn palette_invalid_hex_keeps_default() {
        let colors = config::Colors { foreground: Some("nope".into()), ..Default::default() };
        let palette = Palette::with_overrides(&colors);
        assert_eq!(palette.foreground, config::DEFAULT_FG);
        assert_eq!(Palette::default(), Palette::with_overrides(&config::Colors::default()));
    }

    // -- stale_rows (damage tracking) -----------------------------------------

    fn cache(display_offset: usize, font_size: f32, screen_lines: usize) -> RowCache {
        RowCache { rows: Vec::new(), display_offset, font_size, screen_lines }
    }

    #[test]
    fn stale_rows_full_damage_or_context_change_rebuilds_all() {
        let cache = cache(0, 14.0, 24);
        assert_eq!(stale_rows(&Damage::Full, &cache, 0, 14.0, 24), None);
        // Scrolled, resized or re-fonted cache: full rebuild even on partial damage.
        let partial = Damage::Partial(vec![3]);
        assert_eq!(stale_rows(&partial, &cache, 5, 14.0, 24), None);
        assert_eq!(stale_rows(&partial, &cache, 0, 16.0, 24), None);
        assert_eq!(stale_rows(&partial, &cache, 0, 14.0, 30), None);
    }

    #[test]
    fn stale_rows_partial_damage_rebuilds_only_those_lines() {
        let cache = cache(0, 14.0, 24);
        let damage = Damage::Partial(vec![2, 7, 100]);
        // Out-of-viewport lines are dropped.
        assert_eq!(stale_rows(&damage, &cache, 0, 14.0, 24), Some(vec![2, 7]));
    }

    // -- layout_row -----------------------------------------------------------

    const RED: Color32 = Color32::RED;
    const BLUE: Color32 = Color32::BLUE;

    fn cell(text: &str, fg: Color32) -> RowCell {
        RowCell {
            text: text.into(),
            style: CellStyle { fg, underline: false, strikethrough: false },
        }
    }

    fn font() -> FontId {
        FontId::monospace(config::FONT_SIZE)
    }

    #[test]
    fn row_text_is_concatenated_with_spaces() {
        let cells = [cell("a", RED), cell(" ", RED), cell("b", BLUE)];
        let job = layout_row(&font(), &cells).unwrap();
        assert_eq!(job.text, "a b");
        // Two sections: red "a ", blue "b"; ranges cover the text seamlessly.
        assert_eq!(job.sections.len(), 2);
        assert_eq!(job.sections[0].byte_range, ByteIndex(0)..ByteIndex(2));
        assert_eq!(job.sections[1].byte_range, ByteIndex(2)..ByteIndex(3));
        assert_eq!(job.sections[0].format.color, RED);
        assert_eq!(job.sections[1].format.color, BLUE);
    }

    #[test]
    fn row_of_spaces_is_not_drawn_and_trailing_blanks_are_cut() {
        let cells = [cell(" ", RED), cell(" ", BLUE)];
        assert!(layout_row(&font(), &cells).is_none());

        let cells = [cell("a", RED), cell(" ", RED), cell(" ", RED)];
        let job = layout_row(&font(), &cells).unwrap();
        assert_eq!(job.text, "a");
    }

    #[test]
    fn typed_urls_are_underlined() {
        let text = "go https://x.io now";
        let mut cells: Vec<RowCell> = text.chars().map(|c| cell(&c.to_string(), RED)).collect();
        underline_urls(&mut cells);
        let underlined: String = cells
            .iter()
            .zip(text.chars())
            .filter(|(cell, _)| cell.style.underline)
            .map(|(_, c)| c)
            .collect();
        assert_eq!(underlined, "https://x.io");
        // No URL: styles are untouched.
        let mut cells = vec![cell("a", RED), cell("b", BLUE)];
        underline_urls(&mut cells);
        assert!(!cells.iter().any(|cell| cell.style.underline));
    }

    #[test]
    fn underline_and_strikethrough_split_sections() {
        let mut underlined = cell("u", RED);
        underlined.style.underline = true;
        let mut struck = cell("s", RED);
        struck.style.strikethrough = true;
        let cells = [cell("a", RED), underlined, struck];
        let job = layout_row(&font(), &cells).unwrap();
        assert_eq!(job.text, "aus");
        assert_eq!(job.sections.len(), 3);
        assert_eq!(job.sections[0].format.underline, Stroke::NONE);
        assert_eq!(job.sections[1].format.underline, Stroke::new(1.0, RED));
        assert_eq!(job.sections[2].format.strikethrough, Stroke::new(1.0, RED));
    }
}
