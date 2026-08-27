//! Git-aware prompt: `~/path  branch* ❯` (dir cyan, branch magenta, dirty red).

use std::path::Path;
use std::process::Command;

const CYAN: &str = "\x1b[36m";
const MAGENTA: &str = "\x1b[35m";
const RED: &str = "\x1b[31m";
const RESET: &str = "\x1b[0m";

pub fn render() -> String {
    let cwd = std::env::current_dir().ok();
    let dir_text = cwd.as_deref().map(shortened_cwd).unwrap_or_else(|| "?".into());
    let mut prompt = format!("{CYAN}{dir_text}{RESET}");
    if let Some(cwd) = cwd.as_deref()
        && let Some(git) = git_segment(cwd)
    {
        prompt.push_str("  ");
        prompt.push_str(&git);
    }
    prompt.push_str(" ❯ ");
    prompt
}

/// Cwd with the home directory replaced by `~`.
fn shortened_cwd(cwd: &Path) -> String {
    let Some(home) = std::env::var_os("HOME").map(std::path::PathBuf::from) else {
        return cwd.display().to_string();
    };
    if cwd == home {
        return "~".into();
    }
    match cwd.strip_prefix(&home) {
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => cwd.display().to_string(),
    }
}

/// Git part of the prompt for the repository containing `dir`, if any.
fn git_segment(dir: &Path) -> Option<String> {
    let branch = git_output(dir, &["rev-parse", "--is-inside-work-tree"])?;
    if branch != "true" {
        return None;
    }
    let name = git_output(dir, &["symbolic-ref", "--short", "HEAD"])
        .or_else(|| git_output(dir, &["rev-parse", "--short", "HEAD"]))
        .unwrap_or_else(|| "?".into());
    let dirty =
        git_output(dir, &["status", "--porcelain"]).is_some_and(|status| !status.is_empty());
    let dirty_marker = if dirty { format!("{RED}*{RESET}") } else { String::new() };
    Some(format!("{MAGENTA}{name}{RESET}{dirty_marker}"))
}

/// Stdout of a git call in `dir`, trimmed; None when git fails.
fn git_output(dir: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(dir: &Path, args: &[&str]) -> bool {
        Command::new("git")
            .args(args)
            .current_dir(dir)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }

    #[test]
    fn prompt_outside_repo_has_no_branch() {
        let dir = std::env::temp_dir().join(format!("comma-prompt-plain-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(git_segment(&dir).is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn prompt_in_repo_shows_branch_and_dirty_marker() {
        let dir = std::env::temp_dir().join(format!("comma-prompt-repo-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert!(git(&dir, &["init", "-b", "comma-test-branch"]));

        // Clean repo on an unborn branch: no commits needed for symbolic-ref.
        let segment = git_segment(&dir).expect("expected a git segment");
        assert!(segment.contains("comma-test-branch"), "segment: {segment}");
        assert!(!segment.contains('*'), "unexpected dirty marker: {segment}");

        // Untracked file makes the repo dirty.
        std::fs::write(dir.join("new-file"), "x").unwrap();
        let segment = git_segment(&dir).unwrap();
        assert!(segment.contains('*'), "expected dirty marker: {segment}");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn home_is_shortened_to_tilde() {
        let home = std::env::var("HOME").unwrap();
        let subdir = format!("{home}/some/dir");
        assert_eq!(shortened_cwd(Path::new(&home)), "~");
        assert_eq!(shortened_cwd(Path::new(&subdir)), "~/some/dir");
        // A directory merely sharing a name prefix is not home.
        assert_eq!(shortened_cwd(Path::new(&format!("{home}2"))), format!("{home}2"));
    }
}
