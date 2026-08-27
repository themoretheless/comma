//! A single terminal session tab.

use std::io::Write;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::mpsc::Sender;

use alacritty_terminal::event::Event;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::Term;
use portable_pty::{ChildKiller, MasterPty, PtySize};

use crate::config;
use crate::pty::{self, EventProxy, PtySession, TermSize};
use crate::render;

/// One terminal tab: a PTY session plus its emulated terminal state.
pub(crate) struct Tab {
    id: usize,
    /// Title set by the shell via escape sequences, if any.
    title: Option<String>,
    default_title: String,
    term: Arc<FairMutex<Term<EventProxy>>>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    master: Box<dyn MasterPty + Send>,
    killer: Box<dyn ChildKiller + Send + Sync>,
    /// Shaped text rows reused on frames without terminal damage.
    render_cache: std::cell::RefCell<render::RowCache>,
    /// Set once the child process has exited; the app removes dead tabs.
    dead: bool,
}

impl Tab {
    pub(crate) fn new(
        id: usize,
        sender: Sender<(usize, Event)>,
        ctx: egui::Context,
        size: &TermSize,
        cell_width: f32,
        cell_height: f32,
        config: &config::Config,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let PtySession { term, writer, master, killer } =
            pty::spawn(id, sender, ctx, size, cell_width, cell_height, config)?;
        Ok(Self {
            id,
            title: None,
            default_title: config::default_tab_title(id),
            term,
            writer,
            master,
            killer,
            render_cache: std::cell::RefCell::new(render::RowCache::new()),
            dead: false,
        })
    }

    pub(crate) fn id(&self) -> usize {
        self.id
    }

    pub(crate) fn term(&self) -> &Arc<FairMutex<Term<EventProxy>>> {
        &self.term
    }

    pub(crate) fn render_cache(&self) -> &std::cell::RefCell<render::RowCache> {
        &self.render_cache
    }

    pub(crate) fn label(&self) -> String {
        self.title.clone().unwrap_or_else(|| self.default_title.clone())
    }

    pub(crate) fn set_title(&mut self, title: Option<String>) {
        self.title = title;
    }

    pub(crate) fn is_dead(&self) -> bool {
        self.dead
    }

    pub(crate) fn mark_dead(&mut self) {
        self.dead = true;
    }

    pub(crate) fn write(&self, bytes: &[u8]) {
        if let Ok(mut writer) = self.writer.lock() {
            let _ = writer.write_all(bytes);
            let _ = writer.flush();
        }
    }

    pub(crate) fn resize(&mut self, columns: usize, screen_lines: usize, cell_width: f32, cell_height: f32) {
        {
            let mut term = self.term.lock();
            if term.columns() == columns && term.screen_lines() == screen_lines {
                return;
            }
            term.resize(TermSize::new(columns, screen_lines));
        }
        let _ = self.master.resize(PtySize {
            rows: screen_lines as u16,
            cols: columns as u16,
            pixel_width: cell_width as u16,
            pixel_height: cell_height as u16,
        });
    }
}

impl Drop for Tab {
    fn drop(&mut self) {
        let _ = self.killer.kill();
    }
}
