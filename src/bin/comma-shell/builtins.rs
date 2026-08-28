//! Shell builtins: cd, pwd, echo, exit, export, unset, env, history,
//! jobs, fg, bg.

use std::io::Write;

use crate::exec::{JobState, Shell};

/// All builtin names; `run` and `is_builtin` must stay in sync (tested).
pub const NAMES: &[&str] = &[
    "cd", "pwd", "echo", "exit", "export", "unset", "env", "history", "jobs", "fg", "bg",
    "alias", "unalias",
];

/// Whether `name` is a builtin.
pub fn is_builtin(name: &str) -> bool {
    NAMES.contains(&name)
}

/// Run `argv` as a builtin. Returns `Some(status)` if it was one, else `None`.
/// Output goes to `out` so builtins compose with pipelines and redirects.
pub fn run(shell: &mut Shell, argv: &[String], out: &mut dyn Write) -> Option<i32> {
    let (name, args) = argv.split_first()?;
    let status = match name.as_str() {
        "cd" => cd(shell, args, out),
        "pwd" => pwd(out),
        "echo" => echo(args, out),
        "exit" => exit(shell, args, out),
        "export" => export(shell, args, out),
        "unset" => unset(shell, args),
        "env" => print_env(shell, out),
        "history" => history(shell, out),
        "jobs" => jobs(shell, out),
        "fg" => shell.foreground(args.first().map(String::as_str), out),
        "bg" => shell.background(args.first().map(String::as_str), out),
        "alias" => alias(shell, args, out),
        "unalias" => unalias(shell, args, out),
        _ => return None,
    };
    Some(status)
}

fn fail(out: &mut dyn Write, msg: &str) -> i32 {
    let _ = writeln!(out, "comma-shell: {msg}");
    1
}

fn cd(shell: &mut Shell, args: &[String], out: &mut dyn Write) -> i32 {
    let target = match args.first() {
        Some(dir) => dir.clone(),
        None => match shell.env.get("HOME") {
            Some(home) => home.clone(),
            None => return fail(out, "cd: HOME is not set"),
        },
    };
    if let Err(err) = std::env::set_current_dir(&target) {
        return fail(out, &format!("cd: {target}: {err}"));
    }
    0
}

fn pwd(out: &mut dyn Write) -> i32 {
    match std::env::current_dir() {
        Ok(dir) => {
            let _ = writeln!(out, "{}", dir.display());
            0
        }
        Err(err) => fail(out, &format!("pwd: {err}")),
    }
}

fn echo(args: &[String], out: &mut dyn Write) -> i32 {
    let (newline, args) = match args.first().map(String::as_str) {
        Some("-n") => (false, &args[1..]),
        _ => (true, args),
    };
    let _ = write!(out, "{}", args.join(" "));
    if newline {
        let _ = writeln!(out);
    }
    0
}

fn exit(shell: &mut Shell, args: &[String], out: &mut dyn Write) -> i32 {
    // With live jobs the first `exit` only warns; the next one leaves.
    if !shell.exit_guard(out) {
        return 0;
    }
    shell.should_exit = true;
    args.first().and_then(|code| code.parse().ok()).unwrap_or(0)
}

fn export(shell: &mut Shell, args: &[String], out: &mut dyn Write) -> i32 {
    if args.is_empty() {
        return print_env(shell, out);
    }
    let mut status = 0;
    for arg in args {
        match arg.split_once('=') {
            Some((key, value)) if is_valid_name(key) => {
                shell.env.insert(key.to_string(), value.to_string());
            }
            None if is_valid_name(arg) => {} // Exporting an existing name: no-op.
            _ => status = fail(out, &format!("export: `{arg}': not a valid identifier")),
        }
    }
    status
}

fn unset(shell: &mut Shell, args: &[String]) -> i32 {
    for name in args {
        shell.env.remove(name);
    }
    0
}

fn print_env(shell: &Shell, out: &mut dyn Write) -> i32 {
    let mut entries: Vec<_> = shell.env.iter().collect();
    entries.sort();
    for (key, value) in entries {
        let _ = writeln!(out, "{key}={value}");
    }
    0
}

fn history(shell: &Shell, out: &mut dyn Write) -> i32 {
    for (i, entry) in shell.history.iter().enumerate() {
        let _ = writeln!(out, "{:>5}  {entry}", i + 1);
    }
    0
}

fn jobs(shell: &mut Shell, out: &mut dyn Write) -> i32 {
    shell.reap_jobs();
    for job in &shell.jobs {
        let state = match job.state {
            JobState::Running => "Running",
            JobState::Stopped => "Stopped",
        };
        let _ = writeln!(out, "[{}]  {}  {}", job.id, state, job.command_line);
    }
    0
}

/// `alias` lists all aliases; `alias name` shows one; `alias name=value`
/// defines it (the value keeps its raw text, quotes included).
fn alias(shell: &mut Shell, args: &[String], out: &mut dyn Write) -> i32 {
    let mut status = 0;
    if args.is_empty() {
        let mut entries: Vec<_> = shell.aliases.iter().collect();
        entries.sort();
        for (name, value) in entries {
            let _ = writeln!(out, "alias {name}='{value}'");
        }
        return 0;
    }
    for arg in args {
        match arg.split_once('=') {
            Some((name, value)) if is_valid_name(name) => {
                shell.aliases.insert(name.to_string(), value.to_string());
            }
            Some((name, _)) => {
                status = fail(out, &format!("alias: `{name}': not a valid identifier"));
            }
            None => match shell.aliases.get(arg) {
                Some(value) => {
                    let _ = writeln!(out, "alias {arg}='{value}'");
                }
                None => status = fail(out, &format!("alias: {arg}: not found")),
            },
        }
    }
    status
}

fn unalias(shell: &mut Shell, args: &[String], out: &mut dyn Write) -> i32 {
    let mut status = 0;
    for name in args {
        if name == "-a" {
            shell.aliases.clear();
            continue;
        }
        if shell.aliases.remove(name).is_none() {
            status = fail(out, &format!("unalias: {name}: not found"));
        }
    }
    status
}

fn is_valid_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .enumerate()
            .all(|(i, c)| c.is_ascii_alphabetic() || c == '_' || (i > 0 && c.is_ascii_digit()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shell() -> Shell {
        Shell::with_env([("FOO".to_string(), "bar".to_string())])
    }

    fn capture(shell: &mut Shell, argv: &[&str]) -> (i32, String) {
        let argv: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
        let mut out = Vec::new();
        let status = run(shell, &argv, &mut out).unwrap();
        (status, String::from_utf8(out).unwrap())
    }

    #[test]
    fn echo_works() {
        let (status, text) = capture(&mut shell(), &["echo", "a", "b"]);
        assert_eq!((status, text.as_str()), (0, "a b\n"));
        let (_, text) = capture(&mut shell(), &["echo", "-n", "a"]);
        assert_eq!(text, "a");
    }

    #[test]
    fn export_and_unset() {
        let mut shell = shell();
        let (status, _) = capture(&mut shell, &["export", "A=1", "B=two"]);
        assert_eq!(status, 0);
        assert_eq!(shell.env.get("B").unwrap(), "two");
        let (_, _) = capture(&mut shell, &["unset", "A", "FOO"]);
        assert!(!shell.env.contains_key("A"));
        assert!(!shell.env.contains_key("FOO"));
        let (status, _) = capture(&mut shell, &["export", "1BAD=x"]);
        assert_eq!(status, 1);
    }

    #[test]
    fn env_lists_variables() {
        let (status, text) = capture(&mut shell(), &["env"]);
        assert_eq!(status, 0);
        assert!(text.contains("FOO=bar"));
    }

    #[test]
    fn exit_sets_flag() {
        let mut shell = shell();
        let (status, _) = capture(&mut shell, &["exit", "3"]);
        assert_eq!(status, 3);
        assert!(shell.should_exit);
    }

    #[test]
    fn names_and_dispatch_are_in_sync() {
        for name in NAMES {
            assert!(is_builtin(name));
            let argv = vec![name.to_string()];
            assert!(run(&mut shell(), &argv, &mut Vec::new()).is_some(), "{name} not dispatched");
        }
        assert!(!is_builtin("ls"));
        assert!(run(&mut shell(), &["ls".to_string()], &mut Vec::new()).is_none());
    }

    #[test]
    fn alias_set_list_and_remove() {
        let mut shell = shell();
        let (status, _) = capture(&mut shell, &["alias", "ll=ls -la"]);
        assert_eq!(status, 0);
        assert_eq!(shell.aliases.get("ll").unwrap(), "ls -la");

        let (status, text) = capture(&mut shell, &["alias", "ll"]);
        assert_eq!((status, text.as_str()), (0, "alias ll='ls -la'\n"));

        let (status, text) = capture(&mut shell, &["alias"]);
        assert_eq!(status, 0);
        assert!(text.contains("alias ll='ls -la'"), "listing: {text}");

        let (status, _) = capture(&mut shell, &["unalias", "ll"]);
        assert_eq!(status, 0);
        assert!(!shell.aliases.contains_key("ll"));

        let (status, text) = capture(&mut shell, &["unalias", "ll"]);
        assert_eq!(status, 1);
        assert!(text.contains("not found"), "text: {text}");
        let (status, _) = capture(&mut shell, &["alias", "ll"]);
        assert_eq!(status, 1);
    }

    #[test]
    fn unalias_a_removes_everything() {
        let mut shell = shell();
        let _ = capture(&mut shell, &["alias", "a=one", "b=two"]);
        assert_eq!(shell.aliases.len(), 2);
        let (status, _) = capture(&mut shell, &["unalias", "-a"]);
        assert_eq!(status, 0);
        assert!(shell.aliases.is_empty());
        let (_, text) = capture(&mut shell, &["alias"]);
        assert_eq!(text, "");
    }
}
