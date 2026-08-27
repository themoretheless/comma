//! Application state: tabs, layout, input and event routing.

use std::sync::mpsc::{Receiver, Sender, channel};

use alacritty_terminal::event::Event;
use alacritty_terminal::grid::Scroll;
use alacritty_terminal::term::TermMode;
use egui::{Context, Key, Modifiers, Rect, Sense, Vec2};

use crate::pty::TermSize;
use crate::tab::Tab;
use crate::tabs::Tabs;
use crate::{config, input, render};

pub(crate) struct CommaApp {
    tabs: Tabs<Tab>,
    next_id: usize,
    event_tx: Sender<(usize, Event)>,
    event_rx: Receiver<(usize, Event)>,
    /// Cell metrics from the last frame; a guess until fonts are measured.
    cell_size: Vec2,
    config: config::Config,
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
        while let Ok((tab_id, event)) = self.event_rx.try_recv() {
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
        if let Some(tab) = self.tabs.active_mut() {
            for event in ctx.input(|i| i.events.clone()) {
                match event {
                    egui::Event::Text(text) => Self::handle_text(tab, &text),
                    egui::Event::Paste(text) => Self::handle_paste(tab, &text),
                    egui::Event::Copy => Self::handle_copy(ctx, tab),
                    egui::Event::Key { key, pressed: true, modifiers, .. } => {
                        if Self::is_shortcut(key, &modifiers) {
                            shortcuts.push(key);
                        } else {
                            Self::handle_key(tab, key, modifiers);
                        }
                    }
                    _ => {}
                }
            }

            Self::handle_wheel(ctx, tab, rect, cell_size);
            input::handle_selection(tab.term(), rect, cell_size, response);
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

    /// Scrollback via mouse wheel over the terminal area.
    fn handle_wheel(ctx: &Context, tab: &Tab, rect: Rect, cell_size: Vec2) {
        let hovered = ctx.input(|i| i.pointer.hover_pos()).is_some_and(|pos| rect.contains(pos));
        if !hovered {
            return;
        }
        let delta = ctx.input(|i| i.smooth_scroll_delta.y);
        if delta != 0.0 {
            let mut lines = (delta / cell_size.y).round() as i32;
            if lines == 0 {
                lines = delta.signum() as i32;
            }
            tab.term().lock().scroll_display(Scroll::Delta(lines));
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

        egui::CentralPanel::no_frame().show(ui, |ui| {
            let size = ui.available_size();
            let (rect, response) = ui.allocate_exact_size(size, Sense::click_and_drag());
            self.sync_size(rect);
            self.handle_terminal_input(&ctx, rect, &response);
            if let Some(tab) = self.tabs.active() {
                let mut term = tab.term().lock();
                let mut cache = tab.render_cache().borrow_mut();
                render::draw(ui.painter(), &mut term, rect, self.cell_size, self.config.font_size, &mut cache);
            }
        });
    }
}

fn scroll_to_bottom(tab: &Tab) {
    let mut term = tab.term().lock();
    if term.grid().display_offset() != 0 {
        term.scroll_display(Scroll::Bottom);
    }
}
