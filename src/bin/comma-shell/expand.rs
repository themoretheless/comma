//! Word expansion: `$VAR`, `$?`, leading `~` and globbing (`*`, `?`, `[...]`,
//! `**` recursively). Command substitution (`Part::Subst`) is resolved by the
//! executor before expansion (see `exec::substitute_pipeline`).

use std::collections::HashMap;

use crate::lexer::Part;

/// Glob match options, unix shell behavior: `*`/`?`/`[..]` stay within one
/// path component (only `**` crosses `/`) and never match a leading dot.
const GLOB_OPTIONS: glob::MatchOptions = glob::MatchOptions {
    case_sensitive: true,
    require_literal_separator: true,
    require_literal_leading_dot: true,
};

/// Expand a word to a single string (used for redirect targets: no globbing).
pub fn expand_word(
    parts: &[Part],
    env: &HashMap<String, String>,
    last_status: i32,
) -> String {
    expand_segments(parts, env, last_status).into_iter().map(|(text, _)| text).collect()
}

/// Expand a word into its segments: text plus whether glob metacharacters in
/// it are live (only unquoted `Part::Lit` segments glob, like the shell).
fn expand_segments(
    parts: &[Part],
    env: &HashMap<String, String>,
    last_status: i32,
) -> Vec<(String, bool)> {
    let mut segments = Vec::new();
    for part in parts {
        match part {
            Part::Lit(text) => segments.push((text.clone(), true)),
            Part::QLit(text) => segments.push((text.clone(), false)),
            Part::Var(name) => {
                let value = if name == "?" {
                    last_status.to_string()
                } else {
                    env.get(name).cloned().unwrap_or_default()
                };
                segments.push((value, false));
            }
            Part::Tilde => {
                if let Some(home) = env.get("HOME") {
                    segments.push((home.clone(), false));
                }
            }
            // Substitutions are already resolved by the executor.
            Part::Subst(_) => {}
        }
    }
    segments
}

fn has_glob_chars(text: &str) -> bool {
    text.chars().any(|c| matches!(c, '*' | '?' | '['))
}

/// Expand one word into argv entries: variable/tilde expansion, then
/// globbing. No matches → the word stays as-is (bash default).
fn expand_glob_word(
    parts: &[Part],
    env: &HashMap<String, String>,
    last_status: i32,
) -> Vec<String> {
    let segments = expand_segments(parts, env, last_status);
    let literal: String = segments.iter().map(|(text, _)| text.as_str()).collect();
    if !segments.iter().any(|(text, live)| *live && has_glob_chars(text)) {
        return vec![literal];
    }
    // Segments that must not glob (quotes, variables) are pattern-escaped.
    let pattern: String = segments
        .iter()
        .map(|(text, live)| {
            if *live { text.clone() } else { glob::Pattern::escape(text).to_string() }
        })
        .collect();
    let mut matches: Vec<String> = glob::glob_with(&pattern, GLOB_OPTIONS)
        .map(|paths| {
            paths
                .flatten()
                .map(|path| {
                    let text = path.to_string_lossy().into_owned();
                    text.strip_prefix("./").map_or(text.clone(), str::to_owned)
                })
                .collect()
        })
        .unwrap_or_default();
    matches.sort();
    if matches.is_empty() { vec![literal] } else { matches }
}

pub fn expand_argv(
    argv: &[Vec<Part>],
    env: &HashMap<String, String>,
    last_status: i32,
) -> Vec<String> {
    argv.iter().flat_map(|word| expand_glob_word(word, env, last_status)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env() -> HashMap<String, String> {
        HashMap::from([("HOME".into(), "/home/u".into()), ("FOO".into(), "bar".into())])
    }

    #[test]
    fn expands_vars_and_tilde() {
        assert_eq!(expand_word(&[Part::Var("FOO".into())], &env(), 0), "bar");
        assert_eq!(expand_word(&[Part::Var("MISSING".into())], &env(), 0), "");
        assert_eq!(expand_word(&[Part::Var("?".into())], &env(), 42), "42");
        assert_eq!(
            expand_word(&[Part::Tilde, Part::Lit("/x".into())], &env(), 0),
            "/home/u/x"
        );
        assert_eq!(
            expand_word(&[Part::Lit("a".into()), Part::Var("FOO".into())], &env(), 0),
            "abar"
        );
    }

    /// Expand a command line in `dir` and return the resulting argv.
    /// Callers must hold `CWD_LOCK`: this changes the process cwd.
    fn expand_in(dir: &std::path::Path, line: &str) -> Vec<String> {
        let tokens = crate::lexer::lex(line).unwrap();
        let word = match &tokens[0] {
            crate::lexer::Token::Word(parts) => parts.clone(),
            other => panic!("expected a word, got {other:?}"),
        };
        let cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir).unwrap();
        let expanded = expand_glob_word(&word, &HashMap::new(), 0);
        std::env::set_current_dir(cwd).unwrap();
        expanded
    }

    /// Serializes tests that change the process cwd.
    static CWD_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Temp dir with files: a.rs, b.rs, c.txt, sub/d.toml, sub/deep/e.toml, .hidden
    fn glob_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("comma-glob-{}-{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub/deep")).unwrap();
        for file in ["a.rs", "b.rs", "c.txt", "sub/d.toml", "sub/deep/e.toml", ".hidden"] {
            std::fs::write(dir.join(file), "").unwrap();
        }
        dir
    }

    #[test]
    fn glob_star_and_suffix() {
        let _guard = CWD_LOCK.lock().unwrap();
        let dir = glob_dir("star");
        assert_eq!(expand_in(&dir, "*"), vec!["a.rs", "b.rs", "c.txt", "sub"]);
        assert_eq!(expand_in(&dir, "*.rs"), vec!["a.rs", "b.rs"]);
        // `*` does not match dotfiles without an explicit dot.
        assert!(!expand_in(&dir, "*").contains(&".hidden".to_string()));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn glob_recursive_and_classes() {
        let _guard = CWD_LOCK.lock().unwrap();
        let dir = glob_dir("recursive");
        assert_eq!(expand_in(&dir, "**/*.toml"), vec!["sub/d.toml", "sub/deep/e.toml"]);
        assert_eq!(expand_in(&dir, "?.rs"), vec!["a.rs", "b.rs"]);
        assert_eq!(expand_in(&dir, "[ab].rs"), vec!["a.rs", "b.rs"]);
        // Negation is `[!...]` in the glob crate (not `[^...]`).
        assert_eq!(expand_in(&dir, "[!a]*.rs"), vec!["b.rs"]);
        // Glob in the middle of a word.
        assert_eq!(expand_in(&dir, "sub/*.toml"), vec!["sub/d.toml"]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn glob_no_match_stays_literal_and_quotes_dont_glob() {
        let _guard = CWD_LOCK.lock().unwrap();
        let dir = glob_dir("nomatch");
        assert_eq!(expand_in(&dir, "*.nope"), vec!["*.nope"]);
        assert_eq!(expand_in(&dir, "'*.rs'"), vec!["*.rs"]);
        // Quoted part does not glob, unquoted part does.
        assert_eq!(expand_in(&dir, "a'.'rs"), vec!["a.rs"]);
        // A variable holding glob chars is not globbed either.
        let env = HashMap::from([("G".to_string(), "*.rs".to_string())]);
        assert_eq!(expand_word(&[Part::Var("G".into())], &env, 0), "*.rs");
        std::fs::remove_dir_all(&dir).ok();
    }
}
