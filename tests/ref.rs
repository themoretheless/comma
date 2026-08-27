//! Reference tests: replay recorded terminal sessions through the emulator
//! (ansi::Processor + Term, no PTY) and compare the final grid against a
//! stored snapshot. Regression contour for rendering/emulation changes.
//!
//! Recordings live in `tests/ref/<name>/recording` (raw bytes captured from a
//! real PTY session), expected snapshots in `tests/ref/<name>/grid.txt`.
//! Regenerate snapshots deliberately with: `UPDATE_REF=1 cargo test --test ref`.

use std::path::Path;

use alacritty_terminal::event::VoidListener;
use alacritty_terminal::term::Term;
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::vte::ansi::{Color, NamedColor, Processor};

const COLUMNS: usize = 80;
const LINES: usize = 24;

/// Replay a recording and dump the resulting grid as deterministic text.
fn replay(recording: &[u8]) -> String {
    let mut term = Term::new(
        Default::default(),
        &TermSize::new(COLUMNS, LINES),
        VoidListener,
    );
    let mut processor: Processor = Processor::new();
    processor.advance(&mut term, recording);

    let mut text_rows: Vec<String> = vec![String::new(); LINES];
    let mut color_lines = String::new();
    for indexed in term.renderable_content().display_iter {
        let cell = &indexed.cell;
        let row = indexed.point.line.0 as usize;
        let col = indexed.point.column.0;
        if row < LINES {
            text_rows[row].push(cell.c);
        }
        let default_fg = cell.fg == Color::Named(NamedColor::Foreground);
        let default_bg = cell.bg == Color::Named(NamedColor::Background);
        if !default_fg || !default_bg {
            color_lines.push_str(&format!("r{row} c{col} fg={:?} bg={:?}\n", cell.fg, cell.bg));
        }
    }

    let mut dump = String::from("[text]\n");
    for row in &text_rows {
        dump.push_str(row.trim_end());
        dump.push('\n');
    }
    dump.push_str("[colors]\n");
    dump.push_str(&color_lines);
    dump
}

#[test]
fn ref_recordings_match_snapshots() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/ref");
    let update = std::env::var_os("UPDATE_REF").is_some();

    let mut cases: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    cases.sort();
    assert!(!cases.is_empty(), "no ref cases found in {}", dir.display());

    for case in cases {
        let name = case.file_name().unwrap().to_string_lossy().into_owned();
        let recording = std::fs::read(case.join("recording")).expect("recording missing");
        let snapshot_path = case.join("grid.txt");
        let dump = replay(&recording);

        if update {
            std::fs::write(&snapshot_path, &dump).unwrap();
            continue;
        }
        let expected = std::fs::read_to_string(&snapshot_path)
            .unwrap_or_else(|_| panic!("{name}: snapshot missing (run UPDATE_REF=1 cargo test --test ref)"));
        assert_eq!(dump, expected, "ref case {name} diverged");
    }
    if update {
        panic!("snapshots updated; re-run without UPDATE_REF to verify");
    }
}
