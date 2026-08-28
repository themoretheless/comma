//! comma-shell: a small interactive shell for the comma terminal.

mod builtins;
mod completion;
mod exec;
mod expand;
mod lexer;
mod parser;
mod prompt;

use std::borrow::Cow;
use std::io::IsTerminal;
use std::path::Path;

use exec::Shell;
use lexer::{Part, Token};
use rustyline::completion::{Completer, FilenameCompleter, Pair};
use rustyline::error::ReadlineError;
use rustyline::highlight::{CmdKind, Highlighter};
use rustyline::hint::{Hinter, HistoryHinter};
use rustyline::history::DefaultHistory;
use rustyline::validate::Validator;
use rustyline::{Cmd, Config, Context, Editor, EventHandler, Helper, KeyCode, KeyEvent, Modifiers};

/// Serializes tests that change the process cwd (the expand and exec test
/// modules run in this same test process).
#[cfg(test)]
pub(crate) static CWD_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

const GREEN: &str = "\x1b[32m";
const RED: &str = "\x1b[31m";
const CYAN: &str = "\x1b[36m";
const RESET: &str = "\x1b[0m";

/// rustyline helper: fish-style command validation highlight, history hints
/// and tab completion.
struct ShellHelper {
    hinter: HistoryHinter,
    files: FilenameCompleter,
}

impl ShellHelper {
    fn new() -> Self {
        Self { hinter: HistoryHinter::new(), files: FilenameCompleter::new() }
    }
}

impl Completer for ShellHelper {
    type Candidate = Pair;

    /// Command position: builtins + PATH executables. Anywhere else: file
    /// paths via rustyline's `FilenameCompleter`.
    fn complete(
        &self,
        line: &str,
        pos: usize,
        ctx: &Context<'_>,
    ) -> rustyline::Result<(usize, Vec<Pair>)> {
        // rustyline positions are byte indices; the lexer works in chars.
        let char_pos = line[..pos].chars().count();
        if completion::position(line, char_pos) == completion::Position::Argument {
            return self.files.complete(line, pos, ctx);
        }
        let (start, names) = completion::complete(line, char_pos, &command_names(), &[]);
        let byte_start = line.char_indices().nth(start).map(|(byte, _)| byte).unwrap_or(line.len());
        let pairs = names
            .into_iter()
            .map(|name| Pair { display: name.clone(), replacement: name })
            .collect();
        Ok((byte_start, pairs))
    }
}

impl Highlighter for ShellHelper {
    fn highlight<'l>(&self, line: &'l str, _pos: usize) -> Cow<'l, str> {
        highlight_line(line, command_exists)
    }

    // Re-highlight on every change so command validity colors stay live.
    fn highlight_char(&self, _line: &str, _pos: usize, _kind: CmdKind) -> bool {
        true
    }
}

impl Hinter for ShellHelper {
    type Hint = String;

    fn hint(&self, line: &str, pos: usize, ctx: &Context<'_>) -> Option<String> {
        self.hinter.hint(line, pos, ctx)
    }
}

impl Validator for ShellHelper {}
impl Helper for ShellHelper {}

/// Style `line` for display: known command (builtin or in PATH) in green,
/// unknown in red, shell operators in cyan. The contract with rustyline: the
/// result must have the same display width as `line` — only ANSI wrappers are
/// added, the text itself is never changed. On a lexer error the line is
/// returned unstyled.
fn highlight_line<'l>(line: &'l str, command_exists: impl Fn(&str) -> bool) -> Cow<'l, str> {
    let Ok(spans) = lexer::lex_with_spans(line) else {
        return Cow::Borrowed(line);
    };

    let byte_of = |char_pos: usize| {
        line.char_indices().nth(char_pos).map(|(byte, _)| byte).unwrap_or(line.len())
    };

    let mut styled = String::with_capacity(line.len() + 16);
    let mut cursor = 0; // char index
    let mut expect_command = true;
    for (span, token) in &spans {
        // Copy the gap (whitespace) before this token verbatim.
        if cursor < span.start {
            styled.push_str(&line[byte_of(cursor)..byte_of(span.start)]);
        }
        let (start, end) = (byte_of(span.start), byte_of(span.end));
        cursor = span.end;

        let color = match token {
            Token::Pipe | Token::Semi | Token::And | Token::Or | Token::Amp => {
                expect_command = true;
                Some(CYAN)
            }
            Token::Out { .. } | Token::ErrOut { .. } | Token::In => Some(CYAN),
            Token::Word(parts) if expect_command => {
                expect_command = false;
                // Only fully literal names can be looked up; `$VAR`-built
                // command names stay unstyled.
                match literal_word(parts) {
                    Some(name) => Some(if command_exists(&name) { GREEN } else { RED }),
                    None => None,
                }
            }
            Token::Word(_) => None,
        };

        match color {
            Some(color) => {
                styled.push_str(color);
                styled.push_str(&line[start..end]);
                styled.push_str(RESET);
            }
            None => styled.push_str(&line[start..end]),
        }
    }
    styled.push_str(&line[byte_of(cursor)..]);
    Cow::Owned(styled)
}

/// The text of a word made only of literal parts, else `None`.
fn literal_word(parts: &[Part]) -> Option<String> {
    let mut text = String::new();
    for part in parts {
        match part {
            Part::Lit(lit) | Part::QLit(lit) => text.push_str(lit),
            _ => return None,
        }
    }
    Some(text)
}

/// Whether `name` is a builtin or an executable found in PATH.
fn command_exists(name: &str) -> bool {
    builtins::is_builtin(name) || in_path(name)
}

fn in_path(name: &str) -> bool {
    if name.contains('/') {
        return is_executable(Path::new(name));
    }
    std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|dir| is_executable(&dir.join(name))))
}

/// Builtin names plus every executable found in PATH (for completion).
fn command_names() -> Vec<String> {
    let mut names: Vec<String> = builtins::NAMES.iter().map(|name| name.to_string()).collect();
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            if let Ok(entries) = std::fs::read_dir(&dir) {
                for entry in entries.flatten() {
                    if is_executable(&entry.path()) {
                        names.push(entry.file_name().to_string_lossy().into_owned());
                    }
                }
            }
        }
    }
    names.sort();
    names.dedup();
    names
}

/// Whether `path` is an executable regular file (exec.rs uses this too).
pub(crate) fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        path.metadata().is_ok_and(|meta| meta.is_file() && meta.mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

/// Run an rc file line by line through the executor; errors are reported
/// with the line number and don't abort startup. A missing file is fine.
fn run_rc(shell: &mut Shell, path: &Path) {
    let Ok(text) = std::fs::read_to_string(path) else {
        return;
    };
    for (n, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        match exec::parse_line(shell, line) {
            Ok(script) => {
                exec::execute_script(shell, &script);
            }
            Err(err) => eprintln!("comma-shell: {}:{}: {err}", path.display(), n + 1),
        }
    }
}

fn main() {
    install_signal_handlers();

    let mut shell = Shell::new();

    // Startup file, interactive sessions only (stdin is a tty).
    if std::io::stdin().is_terminal()
        && let Some(home) = std::env::var_os("HOME")
    {
        run_rc(&mut shell, &Path::new(&home).join(".commarc"));
    }

    // Keep 10k history entries (the default 100 truncates long sessions)
    // and skip space-prefixed lines.
    let config = Config::builder()
        .max_history_size(10_000)
        .expect("10_000 is a valid history size")
        .history_ignore_space(true)
        .build();
    let mut rl = match Editor::<ShellHelper, DefaultHistory>::with_config(config) {
        Ok(mut rl) => {
            rl.set_helper(Some(ShellHelper::new()));
            // zsh-style: Up/Down search history by the prefix typed so far
            // (on an empty line this is plain history navigation).
            rl.bind_sequence(
                KeyEvent(KeyCode::Up, Modifiers::NONE),
                EventHandler::Simple(Cmd::HistorySearchBackward),
            );
            rl.bind_sequence(
                KeyEvent(KeyCode::Down, Modifiers::NONE),
                EventHandler::Simple(Cmd::HistorySearchForward),
            );
            rl
        }
        Err(err) => {
            eprintln!("comma-shell: failed to init line editor: {err}");
            std::process::exit(1);
        }
    };

    let history_path = std::env::var("HOME").ok().map(|home| format!("{home}/.comma_history"));
    if let Some(path) = &history_path {
        let _ = rl.load_history(path);
    }

    loop {
        // Reap background jobs (mark stopped, drop finished) before the
        // prompt, so `jobs` output and job ids stay fresh.
        shell.reap_jobs();
        let prompt = prompt::render();
        match rl.readline(&prompt) {
            Ok(line) => {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                let _ = rl.add_history_entry(line);
                shell.history.push(line.to_string());
                match exec::parse_line(&shell, line) {
                    Ok(script) => {
                        exec::execute_script(&mut shell, &script);
                    }
                    Err(err) => eprintln!("comma-shell: {err}"),
                }
                if shell.should_exit {
                    break;
                }
            }
            // Ctrl+C: cancel the current line, keep the shell alive.
            Err(ReadlineError::Interrupted) => {
                println!();
                continue;
            }
            // Ctrl+D: leave the shell.
            Err(ReadlineError::Eof) => break,
            Err(err) => {
                eprintln!("comma-shell: {err}");
                break;
            }
        }
    }

    if let Some(path) = &history_path {
        let _ = rl.save_history(path);
    }
}

/// The shell ignores job-control signals; children restore SIG_DFL via
/// `pre_exec` (see exec.rs), so Ctrl+C/Ctrl+Z hit only the foreground
/// pipeline. SIGTTOU/SIGTTIN must be ignored too, otherwise tcsetpgrp from
/// the shell's own (background) group would stop the shell.
fn install_signal_handlers() {
    #[cfg(unix)]
    unsafe {
        for sig in [libc::SIGINT, libc::SIGQUIT, libc::SIGTSTP, libc::SIGTTOU, libc::SIGTTIN] {
            libc::signal(sig, libc::SIG_IGN);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Strip ANSI SGR sequences from a styled string.
    fn strip_ansi(styled: &str) -> String {
        let mut out = String::new();
        let mut chars = styled.chars();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                for c in chars.by_ref() {
                    if c == 'm' {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    }

    fn known(name: &str) -> bool {
        matches!(name, "echo" | "ls")
    }

    #[test]
    fn highlight_keeps_text_and_width() {
        for line in [
            "echo hello",
            "ls -la | grep x > out",
            "unknown-cmd --flag",
            "echo 'a | b' ; ls",
            "écho unicode-テスト | ls",
            "",
        ] {
            let styled = highlight_line(line, known);
            assert_eq!(strip_ansi(&styled), line, "text changed for {line:?}");
        }
    }

    #[test]
    fn known_command_is_green_unknown_is_red() {
        let styled = highlight_line("echo hi", known);
        assert!(styled.contains("\x1b[32mecho\x1b[0m"), "styled: {styled:?}");

        let styled = highlight_line("nope hi", known);
        assert!(styled.contains("\x1b[31mnope\x1b[0m"), "styled: {styled:?}");
    }

    #[test]
    fn operators_are_cyan_and_command_position_resets() {
        let styled = highlight_line("echo a | ls", known);
        assert!(styled.contains("\x1b[36m|\x1b[0m"), "styled: {styled:?}");
        // After the pipe, `ls` is a command again: green.
        assert!(styled.contains("\x1b[32mls\x1b[0m"), "styled: {styled:?}");
        // But `a` is an argument: no color.
        assert!(!styled.contains("\x1b[32ma\x1b[0m"), "styled: {styled:?}");

        let styled = highlight_line("echo a > out", known);
        assert!(styled.contains("\x1b[36m>\x1b[0m"), "styled: {styled:?}");
        // Redirect target is not a command: no color.
        assert!(!styled.contains("out\x1b[0m"), "styled: {styled:?}");
    }

    #[test]
    fn lexer_errors_stay_unstyled() {
        let styled = highlight_line("echo 'unterminated", known);
        assert_eq!(styled, "echo 'unterminated");
    }

    #[test]
    fn quoted_and_variable_commands() {
        // Quoted name: quotes are part of the span, the whole span is styled.
        let styled = highlight_line("'echo' hi", known);
        assert!(styled.contains("\x1b[32m'echo'\x1b[0m"), "styled: {styled:?}");
        // A name built from a variable cannot be validated: unstyled.
        let styled = highlight_line("$CMD hi", known);
        assert_eq!(styled, "$CMD hi");
    }

    #[test]
    fn rc_file_runs_lines_and_survives_errors() {
        let dir = std::env::temp_dir().join(format!("comma-rc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let rc = dir.join("commarc");
        std::fs::write(&rc, "export RC_ONE=1\n# comment\n\na & b\nexport RC_TWO=two\n").unwrap();

        let mut shell = Shell::with_env(Vec::<(String, String)>::new());
        run_rc(&mut shell, &rc);
        assert_eq!(shell.env.get("RC_ONE").unwrap(), "1");
        // A parse error on one line doesn't stop the rest.
        assert_eq!(shell.env.get("RC_TWO").unwrap(), "two");

        // A missing rc file is not an error.
        run_rc(&mut shell, &dir.join("does-not-exist"));
        std::fs::remove_dir_all(&dir).ok();
    }
}
