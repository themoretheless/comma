//! Application state: tabs, layout, input and event routing.

use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use alacritty_terminal::event::Event;
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::term::TermMode;
use alacritty_terminal::vte::ansi::CursorShape;
use egui::{Context, Key, Modifiers, Rect, Sense, Vec2};

use crate::pty::TermSize;
use crate::tab::Tab;
use crate::tabs::Tabs;
use crate::palette::{Action, Palette};
use crate::{config, input, render};

/// Width of the command gutter left of the terminal grid.
const GUTTER: f32 = 14.0;
/// Padding on the other sides of the grid.
const PADDING: f32 = 6.0;

/// Block-cursor blink: one period visible, one period hidden.
const BLINK_PERIOD: Duration = Duration::from_millis(530);

/// Visible phase of the block-cursor blink at `elapsed` since the last
/// input/output activity (activity resets the phase to "visible").
fn blink_visible(elapsed: Duration) -> bool {
    (elapsed.as_millis() / BLINK_PERIOD.as_millis()).is_multiple_of(2)
}

/// Fold a wheel delta (points) into whole scrollback lines, carrying the
/// fractional rest over to the next event (smooth trackpad scrolling).
fn scroll_lines(remainder: &mut f32, delta: f32, cell_height: f32) -> i32 {
    *remainder += delta;
    let lines = (*remainder / cell_height).trunc() as i32;
    *remainder -= lines as f32 * cell_height;
    lines
}

/// Lines of wheel scrolling folded into one button-64/65 mouse report
/// (xterm semantics), so a trackpad doesn't flood the program with events.
const LINES_PER_WHEEL_REPORT: i32 = 3;

/// Fold wheel lines into batched mouse reports: one report per
/// `LINES_PER_WHEEL_REPORT` lines, the rest carried over to the next event.
/// The result is signed: positive = wheel up (64), negative = down (65).
fn batched_reports(pending: &mut i32, lines: i32) -> i32 {
    *pending += lines;
    let reports = *pending / LINES_PER_WHEEL_REPORT;
    *pending -= reports * LINES_PER_WHEEL_REPORT;
    reports
}

pub(crate) struct CommaApp {
    tabs: Tabs<Tab>,
    next_id: usize,
    event_tx: Sender<(usize, Event)>,
    event_rx: Receiver<(usize, Event)>,
    /// Cell metrics from the last frame; a guess until fonts are measured.
    cell_size: Vec2,
    config: config::Config,
    /// Resolved color set (config overrides applied).
    palette: render::Palette,
    /// Last input/output activity; drives the cursor blink phase.
    blink_epoch: Instant,
    /// Fractional wheel delta carried between events, in points.
    scroll_remainder: f32,
    /// Whole wheel lines carried between events for batched mouse reports.
    wheel_report_lines: i32,
    /// Command palette (Cmd+K), when open.
    command_palette: Option<Palette>,
}

/// Subdirectory names of `dir`, sorted, hidden ones skipped, `..` first.
fn list_dirs(dir: &str) -> Vec<String> {
    let mut dirs: Vec<String> = std::fs::read_dir(dir)
        .map(|read| {
            read.flatten()
                .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
                .filter_map(|entry| entry.file_name().into_string().ok())
                .filter(|name| !name.starts_with('.'))
                .collect()
        })
        .unwrap_or_default();
    dirs.sort();
    dirs.insert(0, "..".to_string());
    dirs
}

/// Cut `text` to `max` characters, ending with `…` when shortened.
fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    text.chars().take(max.saturating_sub(1)).collect::<String>() + "…"
}

/// Single-quote a word for the shell (`'` becomes `'\''`).
fn shell_quote(word: &str) -> String {
    format!("'{}'", word.replace('\'', r"'\''"))
}

impl CommaApp {
    pub(crate) fn new(cc: &eframe::CreationContext<'_>, config: config::Config) -> Self {
        let (event_tx, event_rx) = channel();
        let mut app = Self {
            tabs: Tabs::new(),
            next_id: 0,
            event_tx,
            event_rx,
            cell_size: Vec2::new(config::DEFAULT_CELL_WIDTH, config::DEFAULT_CELL_HEIGHT),
            palette: render::Palette::with_overrides(&config.colors),
            blink_epoch: Instant::now(),
            scroll_remainder: 0.0,
            wheel_report_lines: 0,
            command_palette: None,
            config,
        };
        app.new_tab(&cc.egui_ctx);
        app
    }

    fn new_tab(&mut self, ctx: &Context) {
        let id = self.next_id;
        self.next_id += 1;
        let size = TermSize::new(config::START_COLUMNS, config::START_LINES);
        match Tab::new(
            id,
            self.event_tx.clone(),
            ctx.clone(),
            &size,
            self.cell_size.x,
            self.cell_size.y,
            &self.config,
        ) {
            Ok(tab) => self.tabs.push(tab),
            Err(err) => eprintln!("failed to spawn shell: {err}"),
        }
    }

    fn close_tab(&mut self, index: usize) {
        self.tabs.close(index);
    }

    /// `cd` the active tab's shell into the named subdirectory.
    fn cd_into(&mut self, name: &str) {
        if let Some(tab) = self.tabs.active_mut() {
            tab.write(format!("cd {}\n", shell_quote(name)).as_bytes());
        }
    }

    /// Apply events coming from the terminal reader threads.
    fn handle_events(&mut self, ctx: &Context) {
        let mut had_events = false;
        let active_id = self.tabs.active().map(Tab::id);
        while let Ok((tab_id, event)) = self.event_rx.try_recv() {
            had_events = true;
            let Some(tab) = self.tabs.iter_mut().find(|tab| tab.id() == tab_id) else {
                continue;
            };
            match event {
                Event::Title(title) => tab.set_title(Some(title)),
                Event::ResetTitle => tab.set_title(None),
                Event::Exit | Event::ChildExit(_) => tab.mark_dead(),
                Event::PtyWrite(text) => tab.write(text.as_bytes()),
                Event::ClipboardStore(_, text) => ctx.copy_text(text),
                Event::Wakeup if Some(tab_id) != active_id => tab.set_unseen(true),
                _ => {}
            }
        }

        // Remove tabs whose shell has exited.
        for index in (0..self.tabs.len()).rev() {
            if self.tabs.get(index).is_some_and(Tab::is_dead) {
                self.close_tab(index);
            }
        }
        if self.tabs.is_empty() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        // Output activity resets the blink phase to "visible".
        if had_events {
            self.blink_epoch = Instant::now();
        }
    }

    /// Open the command palette over the active tab's tabs and subdirs.
    fn open_palette(&mut self) {
        let tabs: Vec<String> = self.tabs.iter().map(Tab::name).collect();
        let dirs = self.tabs.active().and_then(Tab::cwd_path).map(|d| list_dirs(&d)).unwrap_or_default();
        self.command_palette = Some(Palette::new(&tabs, &dirs));
    }

    /// App-level shortcuts: Cmd+T/W/K and Cmd+1..9.
    fn handle_shortcut(&mut self, ctx: &Context, key: Key) {
        if let Some(index) = input::digit_index(key) {
            self.tabs.switch(index);
            return;
        }
        match key {
            Key::T => self.new_tab(ctx),
            Key::W => self.close_tab(self.tabs.active_index()),
            Key::K => self.open_palette(),
            Key::C => self.apply_action(ctx, Action::CopyLastOutput),
            Key::ArrowUp => self.apply_action(ctx, Action::PrevCommand),
            Key::ArrowDown => self.apply_action(ctx, Action::NextCommand),
            _ => {}
        }
    }

    fn is_shortcut(key: Key, mods: &Modifiers) -> bool {
        mods.command
            && (matches!(key, Key::T | Key::W | Key::K | Key::ArrowUp | Key::ArrowDown)
                || (key == Key::C && mods.shift)
                || input::digit_index(key).is_some())
    }

    /// Scroll so the prompt of the previous (`up`) or next command sits at
    /// the top of the view; past the last command, back to the bottom.
    fn jump_to_command(tab: &Tab, up: bool) {
        let mut term = tab.term().lock();
        let history = term.grid().history_size();
        let offset = term.grid().display_offset();
        let top = history - offset;
        let target = {
            let blocks = tab.blocks();
            if up { blocks.prompt_before(top) } else { blocks.prompt_after(top) }
        };
        match target {
            Some(line) if line <= history => {
                let delta = (history - line) as i32 - offset as i32;
                term.scroll_display(Scroll::Delta(delta));
            }
            _ if !up => term.scroll_display(Scroll::Bottom),
            _ => {}
        }
    }

    /// Text of the last finished command's output.
    fn last_output(tab: &Tab) -> Option<String> {
        let range = tab.blocks().last_output()?;
        if range.is_empty() {
            return Some(String::new());
        }
        let term = tab.term().lock();
        let history = term.grid().history_size() as i64;
        let line = |abs: usize| abs as i64 - history;
        let (first, last) = (line(range.start), line(range.end - 1));
        if first < -history || last >= term.screen_lines() as i64 {
            return None;
        }
        let start = Point::new(Line(first as i32), Column(0));
        let end = Point::new(Line(last as i32), Column(term.columns() - 1));
        Some(term.bounds_to_string(start, end).trim_end().to_string())
    }

    /// Thin bars left of the terminal marking each command, from its prompt
    /// to its end: green when it succeeded, red when it failed, dim while it
    /// runs.
    fn draw_gutter(&self, painter: &egui::Painter, tab: &Tab, gutter: Rect, top: f32) {
        let term = tab.term().lock();
        let history = term.grid().history_size() as i64;
        let offset = term.grid().display_offset() as i64;
        let cursor = crate::pty::absolute_cursor_line(&term) + 1;
        drop(term);
        let color = |index: usize| {
            let rgb = self.palette.indexed[index];
            egui::Color32::from_rgb(rgb.r, rgb.g, rgb.b)
        };
        let painter = painter.with_clip_rect(gutter);
        for block in tab.blocks().commands() {
            let end = block.end.unwrap_or(cursor);
            let row = |abs: usize| abs as i64 - history + offset;
            let y0 = top + row(block.prompt) as f32 * self.cell_size.y + 2.0;
            let y1 = top + row(end) as f32 * self.cell_size.y - 2.0;
            if y1 < gutter.top() || y0 > gutter.bottom() || y1 <= y0 {
                continue;
            }
            let fill = match block.status {
                Some(0) => color(2).gamma_multiply(0.7),
                Some(_) => color(1),
                None if block.end.is_none() => color(8),
                None => color(8).gamma_multiply(0.5),
            };
            let x = gutter.center().x;
            let bar = Rect::from_min_max(egui::pos2(x - 1.0, y0), egui::pos2(x + 1.0, y1));
            painter.rect_filled(bar, 1.0, fill);
        }
    }

    /// Keyboard, scroll and selection input for the active terminal.
    fn handle_terminal_input(&mut self, ctx: &Context, rect: Rect, response: &egui::Response) {
        let cell_size = self.cell_size;
        let mut shortcuts = Vec::new();
        let mut typed = false;
        let mut scroll_remainder = std::mem::take(&mut self.scroll_remainder);
        let mut wheel_report_lines = std::mem::take(&mut self.wheel_report_lines);
        if let Some(tab) = self.tabs.active_mut() {
            let mode = *tab.term().lock().mode();
            // Mouse reporting: the program asked for mouse events; the
            // encoding is SGR (1006) when negotiated, else legacy X10.
            let mouse_report = mode.intersects(TermMode::MOUSE_MODE);
            let encoding = input::mouse_encoding(mode.contains(TermMode::SGR_MOUSE));
            let shift = ctx.input(|i| i.modifiers.shift);
            for event in ctx.input(|i| i.events.clone()) {
                match event {
                    egui::Event::Text(text) => {
                        typed = true;
                        Self::handle_text(tab, &text);
                    }
                    // IME (CJK input) commits finished text; preedit display
                    // is not rendered.
                    egui::Event::Ime(egui::ImeEvent::Commit(text)) => {
                        typed = true;
                        Self::handle_text(tab, &text);
                    }
                    egui::Event::Paste(text) => {
                        typed = true;
                        Self::handle_paste(tab, &text);
                    }
                    egui::Event::Copy => Self::handle_copy(ctx, tab),
                    egui::Event::PointerButton { pos, button, pressed, .. }
                        if mouse_report && !shift && rect.contains(pos) =>
                    {
                        if let Some(mut cb) = input::mouse_button_cb(button) {
                            if !pressed {
                                cb = 3;
                            }
                            let (col, row) = input::cell_at(pos, rect, cell_size);
                            tab.write(&input::encode_mouse(encoding, cb, col, row, pressed));
                        }
                    }
                    egui::Event::PointerMoved(pos) if mouse_report && rect.contains(pos) => {
                        if let Some(cb) = Self::mouse_motion_cb(ctx, mode) {
                            let (col, row) = input::cell_at(pos, rect, cell_size);
                            tab.write(&input::encode_mouse(encoding, cb, col, row, true));
                        }
                    }
                    egui::Event::MouseWheel { delta, .. } if mouse_report && delta.y != 0.0 => {
                        if let Some(pos) = ctx.input(|i| i.pointer.hover_pos())
                            && rect.contains(pos)
                        {
                            // Accumulate pixel deltas into lines and send one
                            // wheel report per ~3 lines instead of one per
                            // event — a trackpad would flood the program.
                            let lines = scroll_lines(&mut scroll_remainder, delta.y, cell_size.y);
                            let reports = batched_reports(&mut wheel_report_lines, lines);
                            if reports != 0 {
                                let cb = if reports > 0 { 64 } else { 65 };
                                let (col, row) = input::cell_at(pos, rect, cell_size);
                                for _ in 0..reports.abs() {
                                    tab.write(&input::encode_mouse(encoding, cb, col, row, true));
                                }
                            }
                        }
                    }
                    egui::Event::Key { key, pressed: true, modifiers, .. } => {
                        if Self::is_shortcut(key, &modifiers) {
                            shortcuts.push(key);
                        } else {
                            typed = true;
                            Self::handle_key(tab, key, modifiers);
                        }
                    }
                    _ => {}
                }
            }

            // Wheel scrolls the scrollback only when the program didn't take
            // the mouse; same for text selection (still available with Shift).
            if !mouse_report {
                Self::handle_wheel(ctx, tab, rect, cell_size, &mut scroll_remainder, mode);
                Self::handle_links(ctx, tab, rect, cell_size, response);
            }
            if !mouse_report || shift {
                input::handle_selection(tab.term(), rect, cell_size, response);
            }
        }
        self.scroll_remainder = scroll_remainder;
        self.wheel_report_lines = wheel_report_lines;
        // Typing resets the blink phase to "visible".
        if typed {
            self.blink_epoch = Instant::now();
        }

        for key in shortcuts {
            self.handle_shortcut(ctx, key);
        }

        // Drop widget focus grabbed by side panel buttons so keystrokes
        // don't trigger them while typing in the terminal.
        ctx.memory_mut(|mem| {
            if let Some(id) = mem.focused() {
                mem.surrender_focus(id);
            }
        });
    }

    /// SGR code for a pointer move, honoring the requested tracking level:
    /// drag (1002, MOUSE_MOTION) reports moves with a button held as 32+btn;
    /// any-event (1003, MOUSE_DRAG) also reports plain moves as 35.
    fn mouse_motion_cb(ctx: &Context, mode: TermMode) -> Option<u8> {
        let held = [egui::PointerButton::Primary, egui::PointerButton::Middle, egui::PointerButton::Secondary]
            .into_iter()
            .find(|&button| ctx.input(|i| i.pointer.button_down(button)));
        match held {
            Some(button) if mode.intersects(TermMode::MOUSE_MOTION | TermMode::MOUSE_DRAG) => {
                input::mouse_button_cb(button).map(|cb| 32 + cb)
            }
            None if mode.contains(TermMode::MOUSE_DRAG) => Some(35),
            _ => None,
        }
    }

    /// Regular text input: write to the PTY, scroll to bottom.
    fn handle_text(tab: &Tab, text: &str) {
        tab.write(text.as_bytes());
        scroll_to_bottom(tab);
    }

    /// Paste: wrap in bracketed-paste markers when the program enabled the
    /// mode, so it can treat pasted text differently from typed input.
    fn handle_paste(tab: &Tab, text: &str) {
        let bracketed = tab.term().lock().mode().contains(TermMode::BRACKETED_PASTE);
        tab.write(&input::paste_bytes(text, bracketed));
        scroll_to_bottom(tab);
    }

    /// Cmd+C: copy the current selection to the system clipboard.
    fn handle_copy(ctx: &Context, tab: &Tab) {
        let text = tab.term().lock().selection_to_string();
        if let Some(text) = text {
            ctx.copy_text(text);
        }
    }

    /// One key press for the terminal: scroll keys, then escape sequences.
    fn handle_key(tab: &Tab, key: Key, modifiers: Modifiers) {
        let mut term = tab.term().lock();
        if modifiers.shift && key == Key::PageUp {
            term.scroll_display(Scroll::PageUp);
            return;
        }
        if modifiers.shift && key == Key::PageDown {
            term.scroll_display(Scroll::PageDown);
            return;
        }
        let mode = *term.mode();
        let app_cursor = mode.contains(TermMode::APP_CURSOR);
        // Kitty keyboard protocol requested by the running program?
        let kitty_mode = mode.contains(TermMode::DISAMBIGUATE_ESC_CODES);
        if let Some(bytes) = input::key_to_bytes(key, modifiers, app_cursor, kitty_mode) {
            if term.grid().display_offset() != 0 {
                term.scroll_display(Scroll::Bottom);
            }
            drop(term);
            tab.write(&bytes);
        }
    }

    /// Mouse wheel over the terminal area, without mouse reporting: scrolls
    /// the scrollback on the main screen; on the alternate screen sends
    /// arrow-key presses to the program instead (alternate scroll).
    /// Fractional (trackpad) deltas accumulate across events until a line.
    fn handle_wheel(
        ctx: &Context,
        tab: &Tab,
        rect: Rect,
        cell_size: Vec2,
        remainder: &mut f32,
        mode: TermMode,
    ) {
        let hovered = ctx.input(|i| i.pointer.hover_pos()).is_some_and(|pos| rect.contains(pos));
        if !hovered {
            return;
        }
        let delta = ctx.input(|i| i.smooth_scroll_delta.y);
        let lines = scroll_lines(remainder, delta, cell_size.y);
        if lines == 0 {
            return;
        }
        if mode.contains(TermMode::ALT_SCREEN) {
            tab.write(&input::alt_scroll_bytes(lines, mode.contains(TermMode::APP_CURSOR)));
        } else {
            tab.term().lock().scroll_display(Scroll::Delta(lines));
        }
    }

    /// Link interaction: Cmd+hover over an OSC-8 hyperlink or a detected
    /// typed URL shows a pointing hand; a Cmd+click (press+release without
    /// drag, so it doesn't fight selection) opens the URI.
    fn handle_links(ctx: &Context, tab: &Tab, rect: Rect, cell_size: Vec2, response: &egui::Response) {
        if !ctx.input(|i| i.modifiers.command) {
            return;
        }
        let hovered = ctx.input(|i| i.pointer.hover_pos()).filter(|pos| rect.contains(*pos));
        let Some(pos) = hovered else {
            return;
        };
        let uri = input::link_at(&tab.term().lock(), pos, rect, cell_size);
        let Some(uri) = uri else {
            return;
        };
        ctx.output_mut(|o| o.cursor_icon = egui::CursorIcon::PointingHand);
        if response.clicked()
            && let Err(err) = std::process::Command::new("open").arg(&uri).spawn()
        {
            eprintln!("comma: failed to open {uri:?}: {err}");
        }
    }

    /// Resize the active terminal when the viewport area changes.
    fn sync_size(&mut self, rect: Rect) {
        let columns = (rect.width() / self.cell_size.x).floor().max(1.0) as usize;
        let screen_lines = (rect.height() / self.cell_size.y).floor().max(1.0) as usize;
        if let Some(tab) = self.tabs.active_mut() {
            tab.resize(columns, screen_lines, self.cell_size.x, self.cell_size.y);
        }
    }

    /// Tab list: one row per tab — name, then a dim line with the git
    /// branch and the foreground program. A dot marks background output;
    /// the close button only appears on hover.
    fn show_sidebar(&mut self, ui: &mut egui::Ui) {
        let branch_color = {
            let rgb = self.palette.indexed[5];
            egui::Color32::from_rgb(rgb.r, rgb.g, rgb.b)
        };
        let accent = {
            let rgb = self.palette.indexed[6];
            egui::Color32::from_rgb(rgb.r, rgb.g, rgb.b)
        };
        let background = {
            let rgb = self.palette.background;
            egui::Color32::from_rgb(rgb.r, rgb.g, rgb.b)
        };
        if let Some(tab) = self.tabs.active_mut() {
            tab.set_unseen(false);
        }
        egui::Panel::left("tabs")
            .resizable(false)
            .default_size(self.config.sidebar_width)
            .frame(egui::Frame::NONE.fill(background).inner_margin(egui::Margin::symmetric(8, 0)))
            .show(ui, |ui| {
                // Clear the traffic-light buttons in the hidden title bar.
                ui.add_space(36.0);
                let mut close = None;
                let mut switch_to = None;
                for (index, tab) in self.tabs.iter().enumerate() {
                    let selected = index == self.tabs.active_index();
                    let detail = match (tab.git_branch(), tab.running()) {
                        (Some(branch), Some(cmd)) => Some((Some(branch), Some(cmd))),
                        (branch, cmd) if branch.is_some() || cmd.is_some() => Some((branch, cmd)),
                        _ => None,
                    };
                    let height = if detail.is_some() { 40.0 } else { 26.0 };
                    let (rect, response) = ui
                        .allocate_exact_size(Vec2::new(ui.available_width(), height), Sense::click());
                    let visuals = ui.visuals();
                    let fill = if selected {
                        visuals.widgets.active.weak_bg_fill
                    } else if response.hovered() {
                        visuals.widgets.hovered.weak_bg_fill
                    } else {
                        egui::Color32::TRANSPARENT
                    };
                    let painter = ui.painter_at(rect);
                    painter.rect_filled(rect, 6.0, fill);
                    let text = if selected { visuals.strong_text_color() } else { visuals.text_color() };
                    let weak = visuals.weak_text_color();
                    let left = rect.left() + 10.0;
                    let name = truncate(&tab.name(), config::MAX_TAB_LABEL);
                    painter.text(
                        egui::pos2(left, rect.top() + 13.0),
                        egui::Align2::LEFT_CENTER,
                        name,
                        egui::FontId::proportional(13.0),
                        text,
                    );
                    if let Some((branch, cmd)) = detail {
                        let mut job = egui::text::LayoutJob::default();
                        let small = egui::FontId::proportional(11.0);
                        if let Some(branch) = branch {
                            job.append(&truncate(&branch, 18), 0.0, egui::TextFormat::simple(small.clone(), branch_color));
                        }
                        if let Some(cmd) = cmd {
                            let sep = if job.text.is_empty() { "" } else { "  " };
                            job.append(&format!("{sep}▸ {cmd}"), 0.0, egui::TextFormat::simple(small, weak));
                        }
                        let galley = ui.fonts_mut(|f| f.layout_job(job));
                        painter.galley(egui::pos2(left, rect.top() + 21.0), galley, weak);
                    }
                    let right = egui::pos2(rect.right() - 12.0, rect.top() + 13.0);
                    if response.hovered() || ui.rect_contains_pointer(rect) {
                        let close_rect = egui::Rect::from_center_size(right, Vec2::splat(16.0));
                        let over = ui.input(|i| i.pointer.hover_pos()).is_some_and(|p| close_rect.contains(p));
                        painter.text(
                            right,
                            egui::Align2::CENTER_CENTER,
                            "×",
                            egui::FontId::proportional(14.0),
                            if over { text } else { weak },
                        );
                        if over && response.clicked() {
                            close = Some(index);
                        }
                    } else if tab.has_unseen() {
                        painter.circle_filled(right, 3.0, accent);
                    }
                    if response.clicked() && close.is_none() {
                        switch_to = Some(index);
                    }
                    ui.add_space(2.0);
                }
                if let Some(index) = switch_to {
                    self.tabs.switch(index);
                }
                if let Some(index) = close {
                    self.close_tab(index);
                }
            });
    }

    /// The command palette: a query line and the matching entries, top
    /// center over the terminal. Keys go here, not to the PTY, while open.
    fn show_palette(&mut self, ctx: &Context) {
        let Some(palette) = &mut self.command_palette else {
            return;
        };
        let (up, down, enter, escape) = ctx.input_mut(|i| {
            (
                i.consume_key(Modifiers::NONE, Key::ArrowUp),
                i.consume_key(Modifiers::NONE, Key::ArrowDown),
                i.consume_key(Modifiers::NONE, Key::Enter),
                i.consume_key(Modifiers::NONE, Key::Escape),
            )
        });
        if up {
            palette.move_selection(-1);
        }
        if down {
            palette.move_selection(1);
        }
        let mut close = escape;
        let mut chosen = enter.then(|| palette.chosen()).flatten();
        close |= enter;
        // Dim the window behind the palette; a click outside closes it.
        let screen = ctx.content_rect();
        let backdrop = egui::Area::new(egui::Id::new("palette_backdrop"))
            .fixed_pos(screen.min)
            .order(egui::Order::Middle)
            .show(ctx, |ui| {
                let (rect, response) = ui.allocate_exact_size(screen.size(), Sense::click());
                ui.painter().rect_filled(rect, 0.0, egui::Color32::from_black_alpha(96));
                response
            });
        close |= backdrop.inner.clicked();
        egui::Area::new(egui::Id::new("palette"))
            .anchor(egui::Align2::CENTER_TOP, Vec2::new(0.0, 72.0))
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                let frame = egui::Frame::popup(ui.style())
                    .corner_radius(10.0)
                    .inner_margin(egui::Margin::same(8));
                frame.show(ui, |ui| {
                    ui.set_width(460.0);
                    let before = palette.query.clone();
                    let edit = ui.add(
                        egui::TextEdit::singleline(&mut palette.query)
                            .hint_text("Search tabs, folders, actions")
                            .font(egui::FontId::proportional(16.0))
                            .frame(egui::Frame::NONE)
                            .margin(egui::Margin::symmetric(6, 6))
                            .desired_width(f32::INFINITY),
                    );
                    edit.request_focus();
                    if palette.query != before {
                        palette.selected = 0;
                    }
                    let matches = palette.matches();
                    if matches.is_empty() {
                        return;
                    }
                    ui.add_space(4.0);
                    const VISIBLE: usize = 10;
                    let selected = palette.selected;
                    let first = selected.saturating_sub(VISIBLE - 1);
                    for (i, item) in matches.into_iter().enumerate().skip(first).take(VISIBLE) {
                        let (rect, response) =
                            ui.allocate_exact_size(Vec2::new(ui.available_width(), 28.0), Sense::click());
                        let visuals = ui.visuals();
                        let painter = ui.painter_at(rect);
                        if i == selected {
                            painter.rect_filled(rect, 6.0, visuals.selection.bg_fill);
                        } else if response.hovered() {
                            painter.rect_filled(rect, 6.0, visuals.widgets.hovered.weak_bg_fill);
                        }
                        let text = if i == selected { visuals.strong_text_color() } else { visuals.text_color() };
                        painter.text(
                            egui::pos2(rect.left() + 8.0, rect.center().y),
                            egui::Align2::LEFT_CENTER,
                            &item.label,
                            egui::FontId::proportional(14.0),
                            text,
                        );
                        painter.text(
                            egui::pos2(rect.right() - 8.0, rect.center().y),
                            egui::Align2::RIGHT_CENTER,
                            item.hint,
                            egui::FontId::proportional(12.0),
                            visuals.weak_text_color(),
                        );
                        if response.clicked() {
                            chosen = Some(item.action.clone());
                            close = true;
                        }
                    }
                });
            });
        if close {
            self.command_palette = None;
        }
        if let Some(action) = chosen {
            self.apply_action(ctx, action);
        }
    }

    fn apply_action(&mut self, ctx: &Context, action: Action) {
        match action {
            Action::NewTab => self.new_tab(ctx),
            Action::CloseTab => {
                let index = self.tabs.active_index();
                self.close_tab(index);
            }
            Action::CopySelection => {
                if let Some(tab) = self.tabs.active() {
                    Self::handle_copy(ctx, tab);
                }
            }
            Action::ScrollTop => {
                if let Some(tab) = self.tabs.active() {
                    tab.term().lock().scroll_display(Scroll::Top);
                }
            }
            Action::ScrollBottom => {
                if let Some(tab) = self.tabs.active() {
                    scroll_to_bottom(tab);
                }
            }
            Action::Quit => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
            Action::SwitchTab(index) => self.tabs.switch(index),
            Action::CopyLastOutput => {
                if let Some(text) = self.tabs.active().and_then(Self::last_output) {
                    ctx.copy_text(text);
                }
            }
            Action::PrevCommand | Action::NextCommand => {
                if let Some(tab) = self.tabs.active() {
                    Self::jump_to_command(tab, action == Action::PrevCommand);
                }
            }
            Action::Cd(dir) => self.cd_into(&dir),
        }
    }
}

impl eframe::App for CommaApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();

        self.handle_events(&ctx);

        let cell = render::cell_size(&ctx, self.config.font_size);
        if cell.0 > 0.0 && cell.1 > 0.0 {
            self.cell_size = Vec2::new(cell.0, cell.1);
        }

        self.show_sidebar(ui);

        let focused = ctx.input(|i| i.focused);
        let blink_on = blink_visible(self.blink_epoch.elapsed());
        let mut blinking = false;

        egui::CentralPanel::no_frame().show(ui, |ui| {
            let size = ui.available_size();
            let (area, response) = ui.allocate_exact_size(size, Sense::click_and_drag());
            // Breathing room around the grid; the left strip holds the
            // command gutter.
            let gutter = Rect::from_min_max(area.min, egui::pos2(area.left() + GUTTER, area.bottom()));
            let rect = Rect::from_min_max(
                egui::pos2(gutter.right(), area.top() + PADDING),
                egui::pos2(area.right() - PADDING, area.bottom() - PADDING),
            );
            self.sync_size(rect);
            if self.command_palette.is_none() {
                self.handle_terminal_input(&ctx, rect, &response);
            }
            if let Some(tab) = self.tabs.active() {
                self.draw_gutter(ui.painter(), tab, gutter, rect.top());
                let mut term = tab.term().lock();
                let mut cache = tab.render_cache().borrow_mut();
                render::draw(
                    ui.painter(),
                    &mut term,
                    rect,
                    self.cell_size,
                    self.config.font_size,
                    &mut cache,
                    &self.palette,
                    blink_on,
                    focused,
                );
                // Only the block cursor blinks, and only while focused.
                blinking = focused
                    && matches!(term.renderable_content().cursor.shape, CursorShape::Block);
            }
        });

        self.show_palette(&ctx);

        // Wake up exactly at the next blink toggle instead of free-running.
        if blinking {
            let period = BLINK_PERIOD.as_millis() as u64;
            let elapsed = self.blink_epoch.elapsed().as_millis() as u64;
            ctx.request_repaint_after(Duration::from_millis(period - elapsed % period));
        }
    }
}

fn scroll_to_bottom(tab: &Tab) {
    let mut term = tab.term().lock();
    if term.grid().display_offset() != 0 {
        term.scroll_display(Scroll::Bottom);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blink_phase_alternates() {
        let period = BLINK_PERIOD;
        assert!(blink_visible(Duration::ZERO));
        assert!(blink_visible(period - Duration::from_millis(1)));
        assert!(!blink_visible(period));
        assert!(!blink_visible(period * 2 - Duration::from_millis(1)));
        assert!(blink_visible(period * 2));
    }

    #[test]
    fn list_dirs_sorts_and_skips_hidden() {
        let dir = std::env::temp_dir().join(format!("comma-dirs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("beta")).unwrap();
        std::fs::create_dir_all(dir.join("alpha")).unwrap();
        std::fs::create_dir_all(dir.join(".hidden")).unwrap();
        std::fs::write(dir.join("file.txt"), "").unwrap();
        assert_eq!(list_dirs(dir.to_str().unwrap()), ["..", "alpha", "beta"]);
        std::fs::remove_dir_all(&dir).ok();
        // A missing directory lists only `..`.
        assert_eq!(list_dirs("/nonexistent/dir"), [".."]);
    }

    #[test]
    fn truncate_adds_ellipsis() {
        assert_eq!(truncate("comma", 10), "comma");
        assert_eq!(truncate("abcdef", 4), "abc…");
    }

    #[test]
    fn shell_quote_handles_quotes() {
        assert_eq!(shell_quote("plain"), "'plain'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
    }

    #[test]
    fn wheel_reports_are_batched() {
        let mut pending = 0;
        // Two lines don't make a report yet; the third does.
        assert_eq!(batched_reports(&mut pending, 2), 0);
        assert_eq!(batched_reports(&mut pending, 1), 1);
        assert_eq!(pending, 0);
        // Seven lines at once: two reports, one line carried over.
        assert_eq!(batched_reports(&mut pending, 7), 2);
        assert_eq!(pending, 1);
        // Downward lines batch the same way, with a negative sign.
        let mut pending = 0;
        assert_eq!(batched_reports(&mut pending, -2), 0);
        assert_eq!(batched_reports(&mut pending, -4), -2);
        assert_eq!(pending, 0);
        // Opposite directions cancel out.
        let mut pending = 2;
        assert_eq!(batched_reports(&mut pending, -2), 0);
        assert_eq!(pending, 0);
    }

    #[test]
    fn fractional_scroll_accumulates() {
        let mut remainder = 0.0;
        // Three 6px drags with a 17px cell: two lines total, carried over.
        assert_eq!(scroll_lines(&mut remainder, 6.0, 17.0), 0);
        assert_eq!(scroll_lines(&mut remainder, 6.0, 17.0), 0);
        assert_eq!(scroll_lines(&mut remainder, 6.0, 17.0), 1);
        // A big delta still scrolls many lines at once (1px carried over).
        assert_eq!(scroll_lines(&mut remainder, 100.0, 17.0), 5);
        // Negative (upward) deltas accumulate too.
        let mut remainder = 0.0;
        assert_eq!(scroll_lines(&mut remainder, -8.0, 17.0), 0);
        assert_eq!(scroll_lines(&mut remainder, -9.0, 17.0), -1);
        // Opposite directions cancel out.
        let mut remainder = 5.0;
        assert_eq!(scroll_lines(&mut remainder, -5.0, 17.0), 0);
        assert_eq!(remainder, 0.0);
    }
}
