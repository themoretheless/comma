//! A single terminal session tab.

use std::io::Write;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use alacritty_terminal::event::Event;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::Term;
use portable_pty::{ChildKiller, MasterPty, PtySize};

use crate::config;
use crate::pty::{self, EventProxy, PtySession, TermSize};
use crate::render;

/// How often the shell's working directory is re-polled for the tab label.
const CWD_POLL: Duration = Duration::from_millis(500);

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
    /// Pid of the shell process; its cwd is shown under the tab label.
    child_pid: Option<i32>,
    /// Cached cwd/git-branch poll result and when it was taken.
    cwd_cache: std::cell::RefCell<(Instant, Option<String>, Option<String>)>,
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
        let PtySession { term, writer, master, killer, child_pid } =
            pty::spawn(id, sender, ctx, size, cell_width, cell_height, config)?;
        Ok(Self {
            id,
            title: None,
            default_title: config::default_tab_title(id),
            term,
            writer,
            master,
            killer,
            child_pid,
            cwd_cache: std::cell::RefCell::new((Instant::now() - CWD_POLL, None, None)),
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

    /// Working directory of the shell process, re-polled at most every
    /// `CWD_POLL` and shortened to `~` at `$HOME`. Note this tracks the
    /// shell itself, not the program running in the foreground.
    pub(crate) fn cwd(&self) -> Option<String> {
        self.poll_cwd();
        self.cwd_cache.borrow().1.as_deref().map(shorten_home)
    }

    /// Working directory of the shell process (full path, unshortened).
    pub(crate) fn cwd_path(&self) -> Option<String> {
        self.poll_cwd();
        self.cwd_cache.borrow().1.clone()
    }

    /// Git branch of the repository containing the shell's cwd (detached
    /// HEAD shows the short hash), re-polled with the cwd.
    pub(crate) fn git_branch(&self) -> Option<String> {
        self.poll_cwd();
        self.cwd_cache.borrow().2.clone()
    }

    /// Re-poll the shell's cwd and git branch when the cache is stale.
    fn poll_cwd(&self) {
        let mut cache = self.cwd_cache.borrow_mut();
        if cache.0.elapsed() >= CWD_POLL {
            cache.0 = Instant::now();
            cache.1 = self.child_pid.and_then(process_cwd);
            cache.2 = cache.1.as_deref().and_then(find_git_branch);
        }
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

/// Working directory of process `pid` (macOS `proc_pidinfo`).
#[cfg(target_os = "macos")]
fn process_cwd(pid: i32) -> Option<String> {
    let mut info: libc::proc_vnodepathinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_vnodepathinfo>() as i32;
    // SAFETY: `info` is a valid writable buffer of `size` bytes, as the API
    // requires; the returned path is NUL-terminated inside the struct.
    let ret = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDVNODEPATHINFO,
            0,
            &mut info as *mut _ as *mut std::ffi::c_void,
            size,
        )
    };
    if ret <= 0 {
        return None;
    }
    let path = unsafe { std::ffi::CStr::from_ptr(info.pvi_cdir.vip_path.as_ptr().cast()) };
    Some(path.to_string_lossy().into_owned())
}

#[cfg(not(target_os = "macos"))]
fn process_cwd(_pid: i32) -> Option<String> {
    None
}

/// Shorten a leading `$HOME` to `~` for display.
fn shorten_home(path: &str) -> String {
    if let Some(home) = std::env::var_os("HOME") {
        let home = home.to_string_lossy();
        if path == home {
            return "~".to_string();
        }
        if let Some(rest) = path.strip_prefix(home.as_ref())
            && rest.starts_with('/')
        {
            return format!("~{rest}");
        }
    }
    path.to_string()
}

/// Branch of the git repository containing `dir` (searching upwards), read
/// straight from `.git/HEAD`: `ref: refs/heads/<name>` gives the branch, a
/// bare hash (detached HEAD) its first 7 chars. Worktrees, whose `.git` is
/// a `gitdir:` file, are followed. Returns `None` outside a repository.
fn find_git_branch(dir: &str) -> Option<String> {
    for ancestor in std::path::Path::new(dir).ancestors() {
        let dotgit = ancestor.join(".git");
        let head = if dotgit.is_dir() {
            dotgit.join("HEAD")
        } else if dotgit.is_file() {
            // Worktree or submodule: `.git` points at the real gitdir.
            let content = std::fs::read_to_string(&dotgit).ok()?;
            let gitdir = content.trim().strip_prefix("gitdir:")?.trim();
            ancestor.join(gitdir).join("HEAD")
        } else {
            continue;
        };
        let head = std::fs::read_to_string(head).ok()?;
        let head = head.trim();
        return Some(match head.strip_prefix("ref: refs/heads/") {
            Some(branch) => branch.to_string(),
            None => head.chars().take(7).collect(),
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shorten_home_replaces_prefix() {
        let home = std::env::var("HOME").unwrap();
        assert_eq!(shorten_home(&home), "~");
        assert_eq!(shorten_home(&format!("{home}/x")), "~/x");
        assert_eq!(shorten_home("/other/path"), "/other/path");
        // A sibling whose name merely starts with $HOME's text is not $HOME.
        assert_eq!(shorten_home(&format!("{home}x")), format!("{home}x"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn process_cwd_of_self_is_current_dir() {
        let cwd = process_cwd(std::process::id() as i32).unwrap();
        assert_eq!(cwd, std::env::current_dir().unwrap().to_string_lossy());
    }

    /// Temp dir; the caller removes it.
    fn temp_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("comma-tab-{}-{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn git_branch_read_from_head_file() {
        let dir = temp_dir("git");
        std::fs::create_dir(dir.join(".git")).unwrap();
        std::fs::write(dir.join(".git/HEAD"), "ref: refs/heads/feature/x\n").unwrap();
        assert_eq!(find_git_branch(dir.to_str().unwrap()).as_deref(), Some("feature/x"));
        // A subdirectory walks up to the repository root.
        let nested = dir.join("a/b");
        std::fs::create_dir_all(&nested).unwrap();
        assert_eq!(find_git_branch(nested.to_str().unwrap()).as_deref(), Some("feature/x"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn git_branch_detached_and_non_repo() {
        let dir = temp_dir("detached");
        std::fs::create_dir(dir.join(".git")).unwrap();
        std::fs::write(dir.join(".git/HEAD"), "0123456789abcdef\n").unwrap();
        assert_eq!(find_git_branch(dir.to_str().unwrap()).as_deref(), Some("0123456"));
        std::fs::remove_dir_all(&dir).ok();

        let dir = temp_dir("norepo");
        assert_eq!(find_git_branch(dir.to_str().unwrap()), None);
        std::fs::remove_dir_all(&dir).ok();
    }
}
