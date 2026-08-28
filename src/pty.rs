//! PTY spawning and the reader thread feeding bytes into the terminal emulator.

use std::io::Read;
use std::io::Write;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::mpsc::Sender;
use std::thread;
use std::time::{Duration, Instant};

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::{Config as TermConfig, Term};
use alacritty_terminal::vte::ansi::Processor;
use portable_pty::{ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};

use crate::config;

/// Minimum interval between repaint requests from terminal events (~60 fps).
const MIN_FRAME: Duration = Duration::from_millis(16);

/// Bytes fed to the parser per lock acquisition, so the GUI thread can grab
/// the terminal lock between chunks (alacritty's MAX_LOCKED_READ pattern).
const ADVANCE_CHUNK: usize = 16 * 1024;

/// Grid dimensions in cells, as required by `Term::new`/`Term::resize`.
pub(crate) struct TermSize {
    pub columns: usize,
    pub screen_lines: usize,
}

impl TermSize {
    pub fn new(columns: usize, screen_lines: usize) -> Self {
        Self { columns, screen_lines }
    }
}

impl Dimensions for TermSize {
    fn total_lines(&self) -> usize {
        self.screen_lines
    }

    fn screen_lines(&self) -> usize {
        self.screen_lines
    }

    fn columns(&self) -> usize {
        self.columns
    }
}

/// Forwards terminal events to the app thread and triggers repaints.
#[derive(Clone)]
pub(crate) struct EventProxy {
    tab_id: usize,
    sender: Sender<(usize, Event)>,
    ctx: egui::Context,
    /// Last immediate repaint; used to coalesce repaint storms.
    last_repaint: Arc<Mutex<Instant>>,
}

impl EventProxy {
    pub fn new(tab_id: usize, sender: Sender<(usize, Event)>, ctx: egui::Context) -> Self {
        Self { tab_id, sender, ctx, last_repaint: Arc::new(Mutex::new(Instant::now())) }
    }

    /// Repaint immediately if a frame's worth of time has passed, else
    /// schedule a trailing repaint: single output appears at once, while a
    /// flood of output is throttled to ~60 fps.
    fn request_repaint(&self) {
        let Ok(mut last) = self.last_repaint.lock() else {
            self.ctx.request_repaint();
            return;
        };
        let elapsed = last.elapsed();
        if elapsed >= MIN_FRAME {
            *last = Instant::now();
            self.ctx.request_repaint();
        } else {
            self.ctx.request_repaint_after(MIN_FRAME - elapsed);
        }
    }
}

impl EventListener for EventProxy {
    fn send_event(&self, event: Event) {
        if self.sender.send((self.tab_id, event)).is_ok() {
            self.request_repaint();
        }
    }
}

/// Live PTY session: terminal state, input writer and the master handle.
pub(crate) struct PtySession {
    pub term: Arc<FairMutex<Term<EventProxy>>>,
    pub writer: Arc<Mutex<Box<dyn Write + Send>>>,
    pub master: Box<dyn MasterPty + Send>,
    pub killer: Box<dyn ChildKiller + Send + Sync>,
}

/// Which shell to run in a new session, by priority:
/// 1. `COMMA_SHELL` env override, 2. `shell` from `~/.comma.toml`,
/// 3. `comma-shell` binary next to the current executable (also one level up,
///    for test binaries in `deps/`), 4. `$SHELL` or `/bin/zsh` as a fallback.
fn shell_path(config: &config::Config) -> String {
    if let Ok(path) = std::env::var("COMMA_SHELL") {
        return path;
    }
    if let Some(shell) = &config.shell {
        return shell.clone();
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        for candidate in [dir.join("comma-shell"), dir.join("../comma-shell")] {
            if candidate.is_file() {
                return candidate.to_string_lossy().into_owned();
            }
        }
    }
    std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".to_owned())
}

pub(crate) fn spawn(
    tab_id: usize,
    sender: Sender<(usize, Event)>,
    ctx: egui::Context,
    size: &TermSize,
    cell_width: f32,
    cell_height: f32,
    config: &config::Config,
) -> Result<PtySession, Box<dyn std::error::Error + Send + Sync>> {
    let pty_system = native_pty_system();
    let pair = pty_system.openpty(PtySize {
        rows: size.screen_lines as u16,
        cols: size.columns as u16,
        pixel_width: cell_width as u16,
        pixel_height: cell_height as u16,
    })?;

    let shell = shell_path(config);
    let mut cmd = CommandBuilder::new(&shell);
    cmd.env("TERM", "xterm-256color");
    cmd.env("COLORTERM", "truecolor");
    if let Ok(cwd) = std::env::current_dir() {
        cmd.cwd(cwd);
    }

    let mut child = pair.slave.spawn_command(cmd)?;
    let killer = child.clone_killer();
    let mut reader = pair.master.try_clone_reader()?;
    let writer = pair.master.take_writer()?;

    // Dropping the slave side lets the master see EOF once the child exits.
    drop(pair.slave);

    let proxy = EventProxy::new(tab_id, sender, ctx);
    let term_config = TermConfig {
        scrolling_history: config.scrollback_lines,
        kitty_keyboard: true,
        ..Default::default()
    };
    let term = Arc::new(FairMutex::new(Term::new(term_config, size, proxy.clone())));

    let reader_term = term.clone();
    thread::spawn(move || {
        let mut processor: Processor = Processor::new();
        let mut buf = [0u8; 64 * 1024];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    // Feed the parser in chunks, releasing the terminal lock
                    // between them so the GUI can render mid-stream.
                    for chunk in buf[..n].chunks(ADVANCE_CHUNK) {
                        let mut term = reader_term.lock();
                        processor.advance(&mut *term, chunk);
                    }
                }
                Err(_) => break,
            }
        }
        // Shell exited (or the PTY went away): tell the app to close the tab.
        proxy.send_event(Event::Exit);
    });

    // Reap the child to avoid zombie processes.
    thread::spawn(move || {
        let _ = child.wait();
    });

    Ok(PtySession {
        term,
        writer: Arc::new(Mutex::new(writer)),
        master: pair.master,
        killer,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::mpsc::channel;
    use std::time::{Duration, Instant};

    /// Serializes tests that mutate the process environment.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn grid_text(term: &Term<EventProxy>) -> String {
        term.renderable_content()
            .display_iter
            .map(|indexed| indexed.cell.c)
            .collect()
    }

    /// Poll `cond` until it yields a value; panic after 10s.
    fn wait_until<T>(what: &str, mut cond: impl FnMut() -> Option<T>) -> T {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(value) = cond() {
                return value;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Wait until `needle` shows up in the session grid; returns the grid.
    fn wait_for_grid(session: &PtySession, needle: &str) -> String {
        wait_until(&format!("grid containing {needle:?}"), || {
            let grid = grid_text(&session.term.lock());
            grid.contains(needle).then_some(grid)
        })
    }

    fn write_bytes(session: &PtySession, bytes: &[u8]) {
        let mut writer = session.writer.lock().unwrap();
        writer.write_all(bytes).unwrap();
        writer.flush().unwrap();
    }

    /// Spawn a session, check that `echo <marker>` shows up in the grid,
    /// then exit the shell and expect an `Event::Exit`.
    ///
    /// `prompt` is a string the shell prints once ready; input written before
    /// the first prompt may be lost while the line editor initializes.
    fn check_session(marker: &str, prompt: Option<&str>) {
        let (tx, rx) = channel();
        let session =
            spawn(0, tx, egui::Context::default(), &TermSize::new(80, 24), 8.0, 17.0, &config::Config::default()).unwrap();

        if let Some(prompt) = prompt {
            wait_for_grid(&session, prompt);
        }
        write_bytes(&session, format!("echo {marker}\n").as_bytes());

        // The marker appears twice: echoed input, then the command output.
        // Wait for the output so the shell is back at its prompt before we
        // send the next line.
        wait_until("echo output", || {
            let grid = grid_text(&session.term.lock());
            (grid.matches(marker).count() >= 2).then_some(())
        });

        write_bytes(&session, b"exit\n");
        wait_until("exit event", || match rx.try_recv() {
            Ok((_, Event::Exit)) => Some(()),
            Ok(_) | Err(_) => None,
        });
    }

    #[test]
    fn shell_session_echoes_and_exits() {
        let _guard = ENV_LOCK.lock().unwrap();
        // Use a minimal shell to keep the test independent of user configs.
        unsafe { std::env::set_var("COMMA_SHELL", "/bin/sh") };
        check_session("hello-comma-42", None);
        unsafe { std::env::remove_var("COMMA_SHELL") };
    }

    /// End-to-end: the real comma-shell binary runs inside the PTY.
    #[test]
    fn comma_shell_session_echoes_and_exits() {
        let _guard = ENV_LOCK.lock().unwrap();
        unsafe { std::env::set_var("COMMA_SHELL", comma_shell_path()) };
        check_session("hello-comma-shell-42", Some("❯"));
        unsafe { std::env::remove_var("COMMA_SHELL") };
    }

    /// Ctrl+C kills the foreground command but not comma-shell.
    #[test]
    fn comma_shell_survives_ctrl_c() {
        let _guard = ENV_LOCK.lock().unwrap();
        unsafe { std::env::set_var("COMMA_SHELL", comma_shell_path()) };

        let (tx, _rx) = channel();
        let session =
            spawn(0, tx, egui::Context::default(), &TermSize::new(80, 24), 8.0, 17.0, &config::Config::default()).unwrap();

        wait_for_grid(&session, "❯"); // shell is ready for input
        write_bytes(&session, b"sleep 100\n");
        wait_for_grid(&session, "sleep 100"); // input echoed back

        // No fixed sleeps: wait until the sleep process actually runs.
        wait_until("sleep process", || {
            let out = std::process::Command::new("pgrep").args(["-f", "sleep 100"]).output().ok()?;
            (out.status.success() && !out.stdout.is_empty()).then_some(())
        });

        write_bytes(&session, b"\x03"); // Ctrl+C

        // The prompt comes back only once the foreground command is gone.
        wait_until("prompt after Ctrl+C", || {
            let grid = grid_text(&session.term.lock());
            (grid.matches("❯").count() >= 2).then_some(grid)
        });

        write_bytes(&session, b"echo still-alive-7\n");
        let grid = wait_until("echo output after Ctrl+C", || {
            let grid = grid_text(&session.term.lock());
            (grid.matches("still-alive-7").count() >= 2).then_some(grid)
        });
        assert!(grid.contains("still-alive-7"));

        write_bytes(&session, b"exit\n");
        unsafe { std::env::remove_var("COMMA_SHELL") };
    }

    /// Ctrl+Z stops the foreground command but not comma-shell; the shell
    /// must return to the prompt and keep working (job control).
    #[test]
    fn comma_shell_survives_ctrl_z() {
        let _guard = ENV_LOCK.lock().unwrap();
        unsafe { std::env::set_var("COMMA_SHELL", comma_shell_path()) };

        let (tx, _rx) = channel();
        let session =
            spawn(0, tx, egui::Context::default(), &TermSize::new(80, 24), 8.0, 17.0, &config::Config::default()).unwrap();

        wait_for_grid(&session, "❯"); // shell is ready for input
        write_bytes(&session, b"sleep 100\n");
        wait_for_grid(&session, "sleep 100"); // input echoed back

        // No fixed sleeps: wait until the sleep process actually runs.
        wait_until("sleep process", || {
            let out = std::process::Command::new("pgrep").args(["-f", "sleep 100"]).output().ok()?;
            (out.status.success() && !out.stdout.is_empty()).then_some(())
        });

        write_bytes(&session, b"\x1a"); // Ctrl+Z

        // The prompt comes back once the foreground pipeline is stopped.
        wait_until("prompt after Ctrl+Z", || {
            let grid = grid_text(&session.term.lock());
            (grid.matches("❯").count() >= 2).then_some(grid)
        });

        write_bytes(&session, b"echo still-alive-z\n");
        wait_until("echo output after Ctrl+Z", || {
            let grid = grid_text(&session.term.lock());
            (grid.matches("still-alive-z").count() >= 2).then_some(())
        });

        // The stopped sleep is left behind; clean it up explicitly.
        if let Ok(out) = std::process::Command::new("pkill").args(["-f", "sleep 100"]).output() {
            let _ = out;
        }
        write_bytes(&session, b"exit\n");
        unsafe { std::env::remove_var("COMMA_SHELL") };
    }

    /// Background jobs: `sleep 100 &` is listed by `jobs`; a foreground sleep
    /// stopped with Ctrl+Z resumes via `fg`; the shell stays alive.
    #[test]
    fn comma_shell_job_control() {
        let _guard = ENV_LOCK.lock().unwrap();
        unsafe { std::env::set_var("COMMA_SHELL", comma_shell_path()) };

        let (tx, _rx) = channel();
        let session =
            spawn(0, tx, egui::Context::default(), &TermSize::new(80, 24), 8.0, 17.0, &config::Config::default()).unwrap();

        wait_for_grid(&session, "❯"); // shell is ready for input

        // Start a background job and check the job notification `[1] <pgid>`.
        write_bytes(&session, b"sleep 100 &\n");
        wait_for_grid(&session, "[1]");

        // `jobs` lists it as running.
        write_bytes(&session, b"jobs\n");
        wait_until("jobs lists the background sleep", || {
            let grid = grid_text(&session.term.lock());
            (grid.contains("Running") && grid.contains("sleep 100")).then_some(())
        });

        // A foreground sleep stopped with Ctrl+Z becomes a stopped job.
        write_bytes(&session, b"sleep 99\n");
        wait_until("sleep 99 process", || {
            let out = std::process::Command::new("pgrep").args(["-f", "sleep 99"]).output().ok()?;
            (out.status.success() && !out.stdout.is_empty()).then_some(())
        });
        write_bytes(&session, b"\x1a"); // Ctrl+Z
        wait_for_grid(&session, "Stopped");

        // `fg %2` puts it back in the foreground. The echoed label is printed
        // only after the terminal handoff, so once it shows up a Ctrl+C byte
        // becomes SIGINT for the job's group (and kills the sleep, proving fg
        // really foregrounded it).
        write_bytes(&session, b"fg %2\n");
        wait_until("fg echoes the command", || {
            let grid = grid_text(&session.term.lock());
            (grid.matches("sleep 99").count() >= 3).then_some(())
        });
        write_bytes(&session, b"\x03"); // Ctrl+C
        wait_until("sleep 99 is gone", || {
            let out = std::process::Command::new("pgrep").args(["-f", "sleep 99"]).output().ok()?;
            (!out.status.success() || out.stdout.is_empty()).then_some(())
        });

        // The shell survived and still answers.
        write_bytes(&session, b"echo alive-jc\n");
        wait_until("echo output after fg", || {
            let grid = grid_text(&session.term.lock());
            (grid.matches("alive-jc").count() >= 2).then_some(())
        });

        // Clean up the leftover background sleep.
        let _ = std::process::Command::new("pkill").args(["-f", "sleep 100"]).output();
        write_bytes(&session, b"exit\n");
        unsafe { std::env::remove_var("COMMA_SHELL") };
    }

    /// Resolve the comma-shell binary built next to the test binary.
    fn comma_shell_path() -> std::path::PathBuf {
        let deps = std::env::current_exe().unwrap();
        let debug = deps.parent().unwrap().parent().unwrap().to_path_buf();
        let shell = debug.join("comma-shell");
        assert!(shell.is_file(), "comma-shell binary not found at {}", shell.display());
        shell
    }
}
