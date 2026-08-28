//! Word expansion: `$VAR`, `$?`, leading `~`, `$(...)` output, word
//! splitting and globbing (`*`, `?`, `[...]`, `**` recursively), in POSIX
//! order: expand → split → glob. Command substitution itself
//! (`Part::Subst`/`QSubst`) is resolved by the executor before expansion
//! (see `exec::substitute_pipeline`).

use std::collections::HashMap;

use crate::lexer::Part;

/// Glob match options, unix shell behavior: `*`/`?`/`[..]` stay within one
/// path component (only `**` crosses `/`) and never match a leading dot.
const GLOB_OPTIONS: glob::MatchOptions = glob::MatchOptions {
    case_sensitive: true,
    require_literal_separator: true,
    require_literal_leading_dot: true,
};

/// One expanded segment, by how it participates in splitting and globbing.
enum Seg {
    /// Unquoted literal text: joins fields, globs.
    Lit(String),
    /// Quoted text (quotes, `"$VAR"`, `"$()"`, `~`): joins fields literally.
    Quoted(String),
    /// Unquoted `$VAR`/`$()` result: splits on IFS whitespace, then globs.
    Expanded(String),
}

impl Seg {
    fn text(&self) -> &str {
        match self {
            Seg::Lit(text) | Seg::Quoted(text) | Seg::Expanded(text) => text,
        }
    }
}

/// Expand a word to a single string (used for redirect targets: no word
/// splitting, no globbing).
pub fn expand_word(
    parts: &[Part],
    env: &HashMap<String, String>,
    last_status: i32,
) -> String {
    expand_segments(parts, env, last_status).iter().map(Seg::text).collect()
}

/// Expand a word into its segments.
fn expand_segments(
    parts: &[Part],
    env: &HashMap<String, String>,
    last_status: i32,
) -> Vec<Seg> {
    let var = |name: &String| {
        if name == "?" {
            last_status.to_string()
        } else {
            env.get(name).cloned().unwrap_or_default()
        }
    };
    let mut segments = Vec::new();
    for part in parts {
        match part {
            Part::Lit(text) => segments.push(Seg::Lit(text.clone())),
            Part::QLit(text) => segments.push(Seg::Quoted(text.clone())),
            Part::Var(name) => segments.push(Seg::Expanded(var(name))),
            Part::QVar(name) => segments.push(Seg::Quoted(var(name))),
            Part::Tilde => {
                if let Some(home) = env.get("HOME") {
                    segments.push(Seg::Quoted(home.clone()));
                }
            }
            Part::SubstOut(text) => segments.push(Seg::Expanded(text.clone())),
            // Substitutions are already resolved by the executor.
            Part::Subst(_) | Part::QSubst(_) => {}
        }
    }
    segments
}

/// IFS whitespace for word splitting: `$IFS` or the POSIX default.
fn ifs_chars(env: &HashMap<String, String>) -> String {
    env.get("IFS").cloned().unwrap_or_else(|| " \t\n".to_string())
}

/// One output field: its text, its glob pattern (quoted chars escaped) and
/// whether any unquoted segment contributed glob metacharacters.
#[derive(Default)]
struct Field {
    text: String,
    pattern: String,
    has_meta: bool,
}

/// Split segments into fields on IFS whitespace. Literal/quoted segments
/// never split; an `Expanded` segment splits at IFS runs, with the leading
/// piece joining the current field and a trailing IFS closing it (POSIX).
/// An empty expansion contributes nothing; a word that expands to no fields
/// at all disappears entirely.
fn split_fields(segments: Vec<Seg>, ifs: &str) -> Vec<Field> {
    let is_ifs = |c: char| ifs.contains(c);
    let mut fields: Vec<Field> = Vec::new();
    let mut current: Option<Field> = None;
    for seg in segments {
        match seg {
            Seg::Lit(text) => {
                let field = current.get_or_insert_with(Field::default);
                field.has_meta |= has_glob_chars(&text);
                field.text.push_str(&text);
                field.pattern.push_str(&text);
            }
            Seg::Quoted(text) => {
                let field = current.get_or_insert_with(Field::default);
                field.pattern.push_str(&glob::Pattern::escape(&text).to_string());
                field.text.push_str(&text);
            }
            Seg::Expanded(text) => {
                if text.is_empty() {
                    continue;
                }
                let starts_ifs = text.chars().next().is_some_and(is_ifs);
                let ends_ifs = text.chars().last().is_some_and(is_ifs);
                for (i, piece) in
                    text.split(is_ifs).filter(|piece| !piece.is_empty()).enumerate()
                {
                    if i == 0 && !starts_ifs {
                        let field = current.get_or_insert_with(Field::default);
                        field.has_meta |= has_glob_chars(piece);
                        field.text.push_str(piece);
                        field.pattern.push_str(piece);
                    } else {
                        if let Some(field) = current.take() {
                            fields.push(field);
                        }
                        current = Some(Field {
                            text: piece.to_string(),
                            pattern: piece.to_string(),
                            has_meta: has_glob_chars(piece),
                        });
                    }
                }
                if ends_ifs
                    && let Some(field) = current.take()
                {
                    fields.push(field);
                }
            }
        }
    }
    if let Some(field) = current {
        fields.push(field);
    }
    fields
}

fn has_glob_chars(text: &str) -> bool {
    text.chars().any(|c| matches!(c, '*' | '?' | '['))
}

/// Glob one field's pattern; no matches (or no metacharacters) → the field
/// text stays as-is (bash default).
fn glob_field(field: &Field) -> Vec<String> {
    if !field.has_meta {
        return vec![field.text.clone()];
    }
    let mut matches: Vec<String> = glob::glob_with(&field.pattern, GLOB_OPTIONS)
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
    if matches.is_empty() { vec![field.text.clone()] } else { matches }
}

/// Expand one word into argv entries: variable/tilde expansion, word
/// splitting, then globbing.
fn expand_glob_word(
    parts: &[Part],
    env: &HashMap<String, String>,
    last_status: i32,
) -> Vec<String> {
    // An explicitly quoted empty string still yields one (empty) field.
    if parts.is_empty() {
        return vec![String::new()];
    }
    let fields = split_fields(expand_segments(parts, env, last_status), &ifs_chars(env));
    fields.iter().flat_map(glob_field).collect()
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
        assert_eq!(expand_word(&[Part::QVar("FOO".into())], &env(), 0), "bar");
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

    fn env_with(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    fn glob_word(parts: &[Part], env: &HashMap<String, String>) -> Vec<String> {
        expand_glob_word(parts, env, 0)
    }

    #[test]
    fn unquoted_var_splits_on_ifs_whitespace() {
        let env = env_with(&[("X", "a b c"), ("T", "p\tq\nr")]);
        assert_eq!(glob_word(&[Part::Var("X".into())], &env), ["a", "b", "c"]);
        // Tabs and newlines split too.
        assert_eq!(glob_word(&[Part::Var("T".into())], &env), ["p", "q", "r"]);
        // $IFS is honored.
        let env = env_with(&[("IFS", ","), ("X", "a,b c")]);
        assert_eq!(glob_word(&[Part::Var("X".into())], &env), ["a", "b c"]);
    }

    #[test]
    fn quoted_var_does_not_split() {
        let env = env_with(&[("X", "a b")]);
        assert_eq!(glob_word(&[Part::QVar("X".into())], &env), ["a b"]);
        // Mixed word: only the expansion boundary splits.
        let env = env_with(&[("X", " x y")]);
        assert_eq!(
            glob_word(&[Part::Lit("a".into()), Part::Var("X".into())], &env),
            ["a", "x", "y"]
        );
        let env = env_with(&[("X", "x y")]);
        assert_eq!(
            glob_word(&[Part::Lit("a".into()), Part::Var("X".into())], &env),
            ["ax", "y"]
        );
    }

    #[test]
    fn empty_expansion_disappears_unless_quoted() {
        let env = env_with(&[]);
        // Bare `$EMPTY`: the word vanishes (no empty argument).
        assert_eq!(glob_word(&[Part::Var("EMPTY".into())], &env), Vec::<String>::new());
        // Quoted `"$EMPTY"`: one empty argument.
        assert_eq!(glob_word(&[Part::QVar("EMPTY".into())], &env), [""]);
        // Literal parts keep the word alive.
        assert_eq!(
            glob_word(&[Part::Lit("x".into()), Part::Var("EMPTY".into())], &env),
            ["x"]
        );
    }

    #[test]
    fn substitution_output_splits_when_unquoted() {
        let env = env_with(&[]);
        assert_eq!(glob_word(&[Part::SubstOut("a b".into())], &env), ["a", "b"]);
        // In double quotes the executor produces QLit: no splitting.
        assert_eq!(glob_word(&[Part::QLit("a b".into())], &env), ["a b"]);
    }

    /// Expand a command line in `dir` and return the resulting argv.
    /// Callers must hold `CWD_LOCK`: this changes the process cwd.
    fn expand_in(dir: &std::path::Path, line: &str) -> Vec<String> {
        expand_in_env(dir, line, &HashMap::new())
    }

    fn expand_in_env(dir: &std::path::Path, line: &str, env: &HashMap<String, String>) -> Vec<String> {
        let tokens = crate::lexer::lex(line).unwrap();
        let word = match &tokens[0] {
            crate::lexer::Token::Word(parts) => parts.clone(),
            other => panic!("expected a word, got {other:?}"),
        };
        let cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir).unwrap();
        let expanded = expand_glob_word(&word, env, 0);
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
        // Unquoted variable holding glob chars: globbed (POSIX).
        let env = env_with(&[("G", "*.rs")]);
        assert_eq!(expand_in_env(&dir, "$G", &env), vec!["a.rs", "b.rs"]);
        // ...but a redirect target (expand_word) is never globbed or split.
        assert_eq!(expand_word(&[Part::Var("G".into())], &env, 0), "*.rs");
        std::fs::remove_dir_all(&dir).ok();
    }
}
