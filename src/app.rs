//! Application state: tabs, layout, input and event routing.

use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use alacritty_terminal::event::Event;
use alacritty_terminal::grid::Scroll;
use alacritty_terminal::term::TermMode;
use alacritty_terminal::vte::ansi::CursorShape;
use egui::{Context, Key, Modifiers, Rect, Sense, Vec2};

use crate::pty::TermSize;
use crate::tab::Tab;
use crate::tabs::Tabs;
use crate::{config, input, render};

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

    /// Apply events coming from the terminal reader threads.
    fn handle_events(&mut self, ctx: &Context) {
        let mut had_events = false;
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

    /// App-level shortcuts: Cmd+T/W and Cmd+1..9.
    fn handle_shortcut(&mut self, ctx: &Context, key: Key) {
        if let Some(index) = input::digit_index(key) {
            self.tabs.switch(index);
            return;
        }
        match key {
            Key::T => self.new_tab(ctx),
            Key::W => self.close_tab(self.tabs.active_index()),
            _ => {}
        }
    }

    fn is_shortcut(key: Key, mods: &Modifiers) -> bool {
        mods.command && (matches!(key, Key::T | Key::W) || input::digit_index(key).is_some())
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

    fn show_sidebar(&mut self, ui: &mut egui::Ui) {
        egui::Panel::left("tabs")
            .resizable(false)
            .default_size(self.config.sidebar_width)
            .show(ui, |ui| {
                ui.add_space(4.0);
                let mut close = None;
                let mut switch_to = None;
                for (index, tab) in self.tabs.iter().enumerate() {
                    let selected = index == self.tabs.active_index();
                    let mut label = tab.label();
                    if label.chars().count() > config::MAX_TAB_LABEL {
                        label =
                            label.chars().take(config::MAX_TAB_LABEL - 1).collect::<String>() + "…";
                    }
                    ui.horizontal(|ui| {
                        if ui.selectable_label(selected, label).clicked() {
                            switch_to = Some(index);
                        }
                        if ui.small_button("×").clicked() {
                            close = Some(index);
                        }
                    });
                }
                if let Some(index) = switch_to {
                    self.tabs.switch(index);
                }
                if let Some(index) = close {
                    self.close_tab(index);
                }
                ui.separator();
                if ui.button("+ new tab").clicked() {
                    let ctx = ui.ctx().clone();
                    self.new_tab(&ctx);
                }
            });
    }

    /// Bottom bar: a "start menu" with app actions on the left and the
    /// active tab status on the right.
    fn show_bottom_bar(&mut self, ui: &mut egui::Ui) {
        egui::Panel::bottom("start_menu").resizable(false).show(ui, |ui| {
            ui.horizontal(|ui| {
                let ctx = ui.ctx().clone();
                ui.menu_button("❯ comma", |ui| {
                    if ui.button("New tab  ⌘T").clicked() {
                        self.new_tab(&ctx);
                        ui.close();
                    }
                    if ui.button("Close tab  ⌘W").clicked() {
                        let index = self.tabs.active_index();
                        self.close_tab(index);
                        ui.close();
                    }
                    ui.separator();
                    if ui.button("Copy selection  ⌘C").clicked() {
                        if let Some(tab) = self.tabs.active() {
                            Self::handle_copy(&ctx, tab);
                        }
                        ui.close();
                    }
                    ui.separator();
                    if ui.button("Scroll to top").clicked() {
                        if let Some(tab) = self.tabs.active() {
                            tab.term().lock().scroll_display(Scroll::Top);
                        }
                        ui.close();
                    }
                    if ui.button("Scroll to bottom").clicked() {
                        if let Some(tab) = self.tabs.active() {
                            scroll_to_bottom(tab);
                        }
                        ui.close();
                    }
                    ui.separator();
                    if ui.button("Quit").clicked() {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                });
                if let Some(tab) = self.tabs.active() {
                    ui.separator();
                    ui.label(tab.label());
                }
                let status = format!("tab {}/{}", self.tabs.active_index() + 1, self.tabs.len());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.weak(status);
                });
            });
        });
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
        self.show_bottom_bar(ui);

        let focused = ctx.input(|i| i.focused);
        let blink_on = blink_visible(self.blink_epoch.elapsed());
        let mut blinking = false;

        egui::CentralPanel::no_frame().show(ui, |ui| {
            let size = ui.available_size();
            let (rect, response) = ui.allocate_exact_size(size, Sense::click_and_drag());
            self.sync_size(rect);
            self.handle_terminal_input(&ctx, rect, &response);
            if let Some(tab) = self.tabs.active() {
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
