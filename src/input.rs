//! Translation of egui keyboard and pointer events for the terminal.

use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::Term;
use egui::{Key, Modifiers, Pos2, Rect, Vec2};

use crate::pty::EventProxy;

/// Tab index for Cmd+1..=Cmd+9 (Num1 → 0, Num9 → 8).
pub(crate) fn digit_index(key: Key) -> Option<usize> {
    let index = key as usize;
    let first = Key::Num1 as usize;
    let last = Key::Num9 as usize;
    (first..=last).contains(&index).then(|| index - first)
}

/// Bytes for a paste: wrapped in bracketed-paste markers when the program
/// running in the terminal enabled the mode, plain text otherwise.
pub(crate) fn paste_bytes(text: &str, bracketed: bool) -> Vec<u8> {
    if bracketed {
        format!("\x1b[200~{text}\x1b[201~").into_bytes()
    } else {
        text.as_bytes().to_vec()
    }
}

/// SGR mouse report: `CSI < Cb ; Cx ; Cy M` for a press, `m` for a release.
/// `cb` is the button/event code (left=0, middle=1, right=2, release=3,
/// wheel up/down=64/65, motion adds 32); `col`/`row` are 0-based cell
/// coordinates, encoded 1-based.
pub(crate) fn sgr_mouse(cb: u8, col: usize, row: usize, pressed: bool) -> Vec<u8> {
    let kind = if pressed { 'M' } else { 'm' };
    format!("\x1b[<{cb};{};{}{kind}", col + 1, row + 1).into_bytes()
}

/// Mouse report encoding negotiated by the program: SGR (1006) when
/// enabled, else the legacy X10 form. UTF8_MOUSE (1005) accepts the same
/// bytes as X10 for our coordinate range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MouseEncoding {
    Sgr,
    X10,
}

pub(crate) fn mouse_encoding(sgr_mouse_mode: bool) -> MouseEncoding {
    if sgr_mouse_mode { MouseEncoding::Sgr } else { MouseEncoding::X10 }
}

/// Encode one mouse report. Legacy X10 (`CSI M Cb Cx Cy`, bytes = 32 +
/// value) has no release suffix: a release is always reported as cb=3, and
/// coordinates are clamped to 223 (the max encodable cell).
pub(crate) fn encode_mouse(
    encoding: MouseEncoding,
    cb: u8,
    col: usize,
    row: usize,
    pressed: bool,
) -> Vec<u8> {
    match encoding {
        MouseEncoding::Sgr => sgr_mouse(cb, col, row, pressed),
        MouseEncoding::X10 => {
            let cb = if pressed { cb } else { 3 };
            let byte = |v: usize| (v.min(223) + 32) as u8;
            vec![0x1b, b'[', b'M', 32 + cb, byte(col + 1), byte(row + 1)]
        }
    }
}

/// Button code for SGR reporting; buttons we don't report yield `None`.
pub(crate) fn mouse_button_cb(button: egui::PointerButton) -> Option<u8> {
    match button {
        egui::PointerButton::Primary => Some(0),
        egui::PointerButton::Middle => Some(1),
        egui::PointerButton::Secondary => Some(2),
        _ => None,
    }
}

/// 0-based cell under a viewport position (used for mouse reporting, whose
/// coordinates are viewport-relative — unlike selection points).
pub(crate) fn cell_at(pos: Pos2, rect: Rect, cell: Vec2) -> (usize, usize) {
    let col = ((pos.x - rect.left()) / cell.x).max(0.0) as usize;
    let row = ((pos.y - rect.top()) / cell.y).max(0.0) as usize;
    (col, row)
}

/// Byte sequence for a pressed key, or `None` when the key produces regular
/// text (handled via `egui::Event::Text`) or an app-level shortcut.
///
/// `kitty_mode` is the kitty keyboard protocol flag (DISAMBIGUATE and above)
/// requested by the program running in the terminal.
pub(crate) fn key_to_bytes(
    key: Key,
    mods: Modifiers,
    app_cursor_mode: bool,
    kitty_mode: bool,
) -> Option<Vec<u8>> {
    // Command-modified keys are app shortcuts (Cmd+T/W/C/V/1..9).
    if mods.command {
        return None;
    }

    if kitty_mode
        && let Some(bytes) = kitty_csi_u(key, mods)
    {
        return Some(bytes);
    }

    if mods.ctrl
        && let Some(byte) = ctrl_byte(key)
    {
        return Some(vec![byte]);
    }

    let bytes: &[u8] = match key {
        Key::ArrowUp if app_cursor_mode => b"\x1bOA",
        Key::ArrowDown if app_cursor_mode => b"\x1bOB",
        Key::ArrowRight if app_cursor_mode => b"\x1bOC",
        Key::ArrowLeft if app_cursor_mode => b"\x1bOD",
        Key::Home if app_cursor_mode => b"\x1bOH",
        Key::End if app_cursor_mode => b"\x1bOF",
        Key::ArrowUp => b"\x1b[A",
        Key::ArrowDown => b"\x1b[B",
        Key::ArrowRight => b"\x1b[C",
        Key::ArrowLeft => b"\x1b[D",
        Key::Home => b"\x1b[H",
        Key::End => b"\x1b[F",
        Key::Enter => b"\r",
        Key::Backspace => b"\x7f",
        Key::Tab if mods.shift => b"\x1b[Z",
        Key::Tab => b"\t",
        Key::Escape => b"\x1b",
        Key::Delete => b"\x1b[3~",
        Key::Insert => b"\x1b[2~",
        Key::PageUp => b"\x1b[5~",
        Key::PageDown => b"\x1b[6~",
        Key::F1 => b"\x1bOP",
        Key::F2 => b"\x1bOQ",
        Key::F3 => b"\x1bOR",
        Key::F4 => b"\x1bOS",
        Key::F5 => b"\x1b[15~",
        Key::F6 => b"\x1b[17~",
        Key::F7 => b"\x1b[18~",
        Key::F8 => b"\x1b[19~",
        Key::F9 => b"\x1b[20~",
        Key::F10 => b"\x1b[21~",
        Key::F11 => b"\x1b[23~",
        Key::F12 => b"\x1b[24~",
        _ => return None,
    };
    Some(bytes.to_vec())
}

/// Control character for Ctrl+letter and friends (C0 control codes).
fn ctrl_byte(key: Key) -> Option<u8> {
    let byte = match key {
        Key::A => 0x01,
        Key::B => 0x02,
        Key::C => 0x03,
        Key::D => 0x04,
        Key::E => 0x05,
        Key::F => 0x06,
        Key::G => 0x07,
        Key::H => 0x08,
        Key::I => 0x09,
        Key::J => 0x0a,
        Key::K => 0x0b,
        Key::L => 0x0c,
        Key::M => 0x0d,
        Key::N => 0x0e,
        Key::O => 0x0f,
        Key::P => 0x10,
        Key::Q => 0x11,
        Key::R => 0x12,
        Key::S => 0x13,
        Key::T => 0x14,
        Key::U => 0x15,
        Key::V => 0x16,
        Key::W => 0x17,
        Key::X => 0x18,
        Key::Y => 0x19,
        Key::Z => 0x1a,
        Key::Space => 0x00,
        Key::OpenBracket => 0x1b,
        Key::Backslash => 0x1c,
        Key::CloseBracket => 0x1d,
        _ => return None,
    };
    Some(byte)
}

/// Kitty keyboard protocol (`CSI u`) encoding for the DISAMBIGUATE mode.
///
/// Covers letters, digits and Enter/Tab/Escape/Backspace with modifiers;
/// key releases, alternate keys and the full kitty functional key table are
/// intentionally out of scope. Returns `None` for keys that keep their
/// legacy encoding (plain keys, shift-only letters — those produce text).
fn kitty_csi_u(key: Key, mods: Modifiers) -> Option<Vec<u8>> {
    // Kitty modifier encoding: 1 + shift(1) + alt(2) + ctrl(4) (+ super(8),
    // unused: Cmd-modified keys are app shortcuts and never reach here).
    let bits = mods.shift as u32 | (mods.alt as u32) << 1 | (mods.ctrl as u32) << 2;
    if bits == 0 {
        return None;
    }

    let code = match key {
        Key::Enter => 13,
        Key::Tab => 9,
        Key::Escape => 27,
        Key::Backspace => 127,
        key if (Key::A as usize..=Key::Z as usize).contains(&(key as usize)) => {
            // Shift-only letters produce text; report them only with ctrl/alt.
            if !(mods.ctrl || mods.alt) {
                return None;
            }
            u32::from(b'a') + (key as usize - Key::A as usize) as u32
        }
        key if (Key::Num0 as usize..=Key::Num9 as usize).contains(&(key as usize)) => {
            if !(mods.ctrl || mods.alt) {
                return None;
            }
            u32::from(b'0') + (key as usize - Key::Num0 as usize) as u32
        }
        _ => return None,
    };
    Some(format!("\x1b[{code};{}u", bits + 1).into_bytes())
}

/// Grid point under a mouse position, in grid coordinates (scrollback-aware).
pub(crate) fn cell_point(
    pos: Pos2,
    rect: Rect,
    cell: Vec2,
    display_offset: usize,
    columns: usize,
) -> Point {
    let col = ((pos.x - rect.left()) / cell.x).max(0.0) as usize;
    let line = ((pos.y - rect.top()) / cell.y).max(0.0) as i32;
    Point::new(
        Line(line - display_offset as i32),
        Column(col.min(columns.saturating_sub(1))),
    )
}

/// Which half of the cell the drag started in.
fn drag_side(pos: Pos2, rect: Rect, cell: Vec2) -> Side {
    if (pos.x - rect.left()) % cell.x < cell.x / 2.0 { Side::Left } else { Side::Right }
}

/// Mouse selection: drag creates/extends a simple selection, click clears it.
pub(crate) fn handle_selection(
    term: &FairMutex<Term<EventProxy>>,
    rect: Rect,
    cell: Vec2,
    response: &egui::Response,
) {
    if response.drag_started()
        && let Some(pos) = response.interact_pointer_pos()
    {
        let mut term = term.lock();
        let point = cell_point(pos, rect, cell, term.grid().display_offset(), term.columns());
        let side = drag_side(pos, rect, cell);
        term.selection = Some(Selection::new(SelectionType::Simple, point, side));
    } else if response.dragged()
        && let Some(pos) = response.interact_pointer_pos()
    {
        let mut term = term.lock();
        let display_offset = term.grid().display_offset();
        let columns = term.columns();
        if let Some(selection) = term.selection.as_mut() {
            let point = cell_point(pos, rect, cell, display_offset, columns);
            selection.update(point, Side::Right);
        }
    } else if response.clicked() {
        term.lock().selection = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONE: Modifiers = Modifiers::NONE;
    const CTRL: Modifiers = Modifiers::CTRL;
    const CMD: Modifiers = Modifiers::COMMAND;
    const SHIFT: Modifiers = Modifiers::SHIFT;

    fn bytes(key: Key, mods: Modifiers, app_cursor: bool) -> Option<Vec<u8>> {
        key_to_bytes(key, mods, app_cursor, false)
    }

    fn kitty_bytes(key: Key, mods: Modifiers) -> Option<Vec<u8>> {
        key_to_bytes(key, mods, false, true)
    }

    #[test]
    fn arrows_respect_app_cursor_mode() {
        for (key, normal, app) in [
            (Key::ArrowUp, "\x1b[A", "\x1bOA"),
            (Key::ArrowDown, "\x1b[B", "\x1bOB"),
            (Key::ArrowRight, "\x1b[C", "\x1bOC"),
            (Key::ArrowLeft, "\x1b[D", "\x1bOD"),
            (Key::Home, "\x1b[H", "\x1bOH"),
            (Key::End, "\x1b[F", "\x1bOF"),
        ] {
            assert_eq!(bytes(key, NONE, false).as_deref(), Some(normal.as_bytes()));
            assert_eq!(bytes(key, NONE, true).as_deref(), Some(app.as_bytes()));
        }
    }

    #[test]
    fn special_keys() {
        for (key, expected) in [
            (Key::Enter, "\r"),
            (Key::Backspace, "\x7f"),
            (Key::Tab, "\t"),
            (Key::Escape, "\x1b"),
            (Key::Delete, "\x1b[3~"),
            (Key::Insert, "\x1b[2~"),
            (Key::PageUp, "\x1b[5~"),
            (Key::PageDown, "\x1b[6~"),
        ] {
            assert_eq!(bytes(key, NONE, false).as_deref(), Some(expected.as_bytes()));
        }
        // BackTab.
        assert_eq!(bytes(Key::Tab, SHIFT, false).as_deref(), Some(b"\x1b[Z".as_slice()));
    }

    #[test]
    fn function_keys() {
        for (key, expected) in [
            (Key::F1, "\x1bOP"),
            (Key::F2, "\x1bOQ"),
            (Key::F3, "\x1bOR"),
            (Key::F4, "\x1bOS"),
            (Key::F5, "\x1b[15~"),
            (Key::F6, "\x1b[17~"),
            (Key::F7, "\x1b[18~"),
            (Key::F8, "\x1b[19~"),
            (Key::F9, "\x1b[20~"),
            (Key::F10, "\x1b[21~"),
            (Key::F11, "\x1b[23~"),
            (Key::F12, "\x1b[24~"),
        ] {
            assert_eq!(bytes(key, NONE, false).as_deref(), Some(expected.as_bytes()));
        }
    }

    #[test]
    fn ctrl_letters_produce_control_codes() {
        for (key, byte) in [
            (Key::A, 0x01),
            (Key::C, 0x03),
            (Key::D, 0x04),
            (Key::L, 0x0c),
            (Key::W, 0x17),
            (Key::Z, 0x1a),
            (Key::Space, 0x00),
            (Key::OpenBracket, 0x1b),
            (Key::Backslash, 0x1c),
            (Key::CloseBracket, 0x1d),
        ] {
            assert_eq!(bytes(key, CTRL, false), Some(vec![byte]));
        }
        // All letters map to 0x01..=0x1a in order.
        for (i, key) in [
            Key::A, Key::B, Key::C, Key::D, Key::E, Key::F, Key::G, Key::H, Key::I, Key::J,
            Key::K, Key::L, Key::M, Key::N, Key::O, Key::P, Key::Q, Key::R, Key::S, Key::T,
            Key::U, Key::V, Key::W, Key::X, Key::Y, Key::Z,
        ]
        .into_iter()
        .enumerate()
        {
            assert_eq!(bytes(key, CTRL, false), Some(vec![i as u8 + 1]));
        }
    }

    #[test]
    fn command_keys_are_app_shortcuts() {
        assert_eq!(bytes(Key::T, CMD, false), None);
        assert_eq!(bytes(Key::C, CMD, false), None);
        assert_eq!(bytes(Key::ArrowUp, CMD, false), None);
    }

    #[test]
    fn sgr_mouse_encoding() {
        for (cb, col, row, pressed, expected) in [
            (0, 0, 0, true, "\x1b[<0;1;1M"),      // left press, top-left cell
            (3, 0, 0, false, "\x1b[<3;1;1m"),     // release
            (1, 4, 2, true, "\x1b[<1;5;3M"),      // middle press
            (2, 79, 23, true, "\x1b[<2;80;24M"),  // right press, bottom-right
            (64, 5, 2, true, "\x1b[<64;6;3M"),    // wheel up
            (65, 5, 2, true, "\x1b[<65;6;3M"),    // wheel down
            (35, 10, 7, true, "\x1b[<35;11;8M"),  // motion, no button
            (32, 10, 7, true, "\x1b[<32;11;8M"),  // drag with left button
        ] {
            assert_eq!(sgr_mouse(cb, col, row, pressed), expected.as_bytes());
        }
    }

    #[test]
    fn mouse_button_codes() {
        assert_eq!(mouse_button_cb(egui::PointerButton::Primary), Some(0));
        assert_eq!(mouse_button_cb(egui::PointerButton::Middle), Some(1));
        assert_eq!(mouse_button_cb(egui::PointerButton::Secondary), Some(2));
        assert_eq!(mouse_button_cb(egui::PointerButton::Extra1), None);
    }

    #[test]
    fn encode_mouse_picks_encoding_by_mode() {
        assert_eq!(mouse_encoding(true), MouseEncoding::Sgr);
        assert_eq!(mouse_encoding(false), MouseEncoding::X10);
        for (encoding, cb, col, row, pressed, expected) in [
            (MouseEncoding::Sgr, 0, 0, 0, true, &b"\x1b[<0;1;1M"[..]),
            (MouseEncoding::Sgr, 3, 0, 0, false, b"\x1b[<3;1;1m"),
            // X10: bytes are 32 + value, releases always cb=3 with 'M'.
            (MouseEncoding::X10, 0, 0, 0, true, b"\x1b[M !!"),
            (MouseEncoding::X10, 0, 0, 0, false, b"\x1b[M#!!"),
            (MouseEncoding::X10, 64, 9, 4, true, b"\x1b[M`*%"),
        ] {
            assert_eq!(encode_mouse(encoding, cb, col, row, pressed), expected);
        }
        // Coordinates beyond 223 cells are clamped in X10.
        assert_eq!(encode_mouse(MouseEncoding::X10, 0, 300, 300, true), b"\x1b[M \xff\xff");
    }

    #[test]
    fn paste_is_bracketed_only_when_the_mode_is_on() {
        for (bracketed, expected) in [
            (true, "\x1b[200~hello\nworld\x1b[201~"),
            (false, "hello\nworld"),
        ] {
            assert_eq!(paste_bytes("hello\nworld", bracketed), expected.as_bytes());
        }
    }

    #[test]
    fn digit_tab_shortcuts() {
        assert_eq!(digit_index(Key::Num1), Some(0));
        assert_eq!(digit_index(Key::Num9), Some(8));
        assert_eq!(digit_index(Key::Num0), None);
        assert_eq!(digit_index(Key::A), None);
    }

    #[test]
    fn printable_keys_produce_text_events_not_bytes() {
        assert_eq!(bytes(Key::A, NONE, false), None);
        assert_eq!(bytes(Key::Space, NONE, false), None);
    }

    #[test]
    fn kitty_mode_encodes_modified_keys_as_csi_u() {
        // Shift+Enter / Ctrl+Enter.
        assert_eq!(kitty_bytes(Key::Enter, SHIFT), Some(b"\x1b[13;2u".to_vec()));
        assert_eq!(kitty_bytes(Key::Enter, CTRL), Some(b"\x1b[13;5u".to_vec()));
        // Ctrl+I is distinct from Tab.
        assert_eq!(kitty_bytes(Key::I, CTRL), Some(b"\x1b[105;5u".to_vec()));
        // Modified Tab/Escape/Backspace.
        assert_eq!(kitty_bytes(Key::Tab, SHIFT), Some(b"\x1b[9;2u".to_vec()));
        assert_eq!(kitty_bytes(Key::Escape, CTRL), Some(b"\x1b[27;5u".to_vec()));
        assert_eq!(kitty_bytes(Key::Backspace, SHIFT), Some(b"\x1b[127;2u".to_vec()));
        // Modifier bits add up: ctrl+shift+a = 1 + 1 + 4 = 6.
        let ctrl_shift = Modifiers::CTRL | Modifiers::SHIFT;
        assert_eq!(kitty_bytes(Key::A, ctrl_shift), Some(b"\x1b[97;6u".to_vec()));
        // Digits with ctrl.
        assert_eq!(kitty_bytes(Key::Num1, CTRL), Some(b"\x1b[49;5u".to_vec()));
    }

    #[test]
    fn kitty_mode_keeps_legacy_for_unmodified_keys() {
        // No modifiers: same bytes as without the protocol.
        assert_eq!(kitty_bytes(Key::Enter, NONE), Some(b"\r".to_vec()));
        assert_eq!(kitty_bytes(Key::Tab, NONE), Some(b"\t".to_vec()));
        assert_eq!(kitty_bytes(Key::Escape, NONE), Some(b"\x1b".to_vec()));
        assert_eq!(kitty_bytes(Key::Backspace, NONE), Some(b"\x7f".to_vec()));
        // Shift-only letters still produce text events, not CSI u.
        assert_eq!(kitty_bytes(Key::A, SHIFT), None);
        // Arrows keep their legacy encoding in this subset.
        assert_eq!(kitty_bytes(Key::ArrowUp, NONE), Some(b"\x1b[A".to_vec()));
    }
}
