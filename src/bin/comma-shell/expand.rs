//! Word expansion: `$VAR`, `${...}` forms, `$?`, leading `~`, `$(...)` and
//! `` `...` `` output, word splitting and globbing (`*`, `?`, `[...]` with
//! `[!]`/`[^]` negation, `**` recursively), in POSIX order: expand → split →
//! glob. Command substitution itself (`Part::Subst`/`QSubst`) is resolved by
//! the executor before expansion (see `exec::substitute_pipeline`).

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
    let var = |name: &String| param_value(name, env, last_status).unwrap_or_default();
    let mut segments = Vec::new();
    for part in parts {
        match part {
            Part::Lit(text) => segments.push(Seg::Lit(text.clone())),
            Part::QLit(text) => segments.push(Seg::Quoted(text.clone())),
            Part::Var(name) => segments.push(Seg::Expanded(var(name))),
            Part::QVar(name) => segments.push(Seg::Quoted(var(name))),
            Part::Param(body) => {
                segments.push(Seg::Expanded(expand_param(body, env, last_status)))
            }
            Part::QParam(body) => {
                segments.push(Seg::Quoted(expand_param(body, env, last_status)))
            }
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

/// Value of a variable reference: `?` is the last exit status.
fn param_value(name: &str, env: &HashMap<String, String>, last_status: i32) -> Option<String> {
    if name == "?" { Some(last_status.to_string()) } else { env.get(name).cloned() }
}

/// Evaluate a `${...}` body: plain `${NAME}`, length `${#NAME}`, and the
/// POSIX default/alternative forms `${NAME:-word}`, `${NAME-word}`,
/// `${NAME:+word}`, `${NAME+word}` (`NAME` may be `?`). The word is
/// recursively expanded by [`expand_braced_word`]. Unknown forms expand to
/// nothing.
fn expand_param(body: &str, env: &HashMap<String, String>, last_status: i32) -> String {
    if let Some(name) = body.strip_prefix('#') {
        return param_value(name, env, last_status)
            .map_or_else(|| "0".into(), |v| v.chars().count().to_string());
    }
    // The name is the longest valid identifier prefix; the rest is the
    // operator and its word.
    let name_len = body
        .char_indices()
        .take_while(|(_, c)| c.is_ascii_alphanumeric() || matches!(c, '_' | '?'))
        .map(|(i, c)| i + c.len_utf8())
        .last()
        .unwrap_or(0);
    let (name, rest) = body.split_at(name_len);
    if name.is_empty() {
        return String::new();
    }
    if rest.is_empty() {
        return param_value(name, env, last_status).unwrap_or_default();
    }
    // `:-`/`:+` test "unset or null"; bare `-`/`+` test only "unset".
    let (test_null, rest) = match rest.strip_prefix(':') {
        Some(rest) => (true, rest),
        None => (false, rest),
    };
    let op = rest.chars().next().unwrap_or(' ');
    let word = &rest[op.len_utf8()..];
    let val = param_value(name, env, last_status);
    let use_value = if test_null { val.as_ref().is_some_and(|v| !v.is_empty()) } else { val.is_some() };
    match op {
        '-' if !use_value => expand_braced_word(word, env, last_status),
        '-' => val.unwrap_or_default(),
        '+' if use_value => expand_braced_word(word, env, last_status),
        _ => String::new(),
    }
}

/// Expand the word of a `${VAR:-word}`-style operator: backslash escapes,
/// quote removal, `$VAR` and nested `${...}` — but no command substitution
/// and no field splitting (the word is a single value by definition).
fn expand_braced_word(word: &str, env: &HashMap<String, String>, last_status: i32) -> String {
    let chars: Vec<char> = word.chars().collect();
    expand_word_span(&chars, 0, env, last_status, None).0
}

/// Scan `chars` from `i`, expanding variables until the `stop` quote (or the
/// end); returns the expanded text and the next index.
fn expand_word_span(
    chars: &[char],
    mut i: usize,
    env: &HashMap<String, String>,
    last_status: i32,
    stop: Option<char>,
) -> (String, usize) {
    let mut out = String::new();
    while i < chars.len() {
        let c = chars[i];
        if Some(c) == stop {
            return (out, i + 1);
        }
        match c {
            '\\' => {
                if let Some(&next) = chars.get(i + 1) {
                    out.push(next);
                    i += 2;
                } else {
                    i += 1;
                }
            }
            '\'' if stop.is_none() => {
                // Single quotes: literal until the closing quote.
                i += 1;
                while let Some(&c) = chars.get(i) {
                    i += 1;
                    if c == '\'' {
                        break;
                    }
                    out.push(c);
                }
            }
            '"' if stop.is_none() => {
                let (inner, next) = expand_word_span(chars, i + 1, env, last_status, Some('"'));
                out.push_str(&inner);
                i = next;
            }
            '$' if chars.get(i + 1) == Some(&'{') => {
                // Matching close with nesting; unbalanced → literal `$`.
                let mut depth = 1;
                let mut j = i + 2;
                while j < chars.len() && depth > 0 {
                    match chars[j] {
                        '{' => depth += 1,
                        '}' => depth -= 1,
                        _ => {}
                    }
                    j += 1;
                }
                if depth == 0 {
                    let body: String = chars[i + 2..j - 1].iter().collect();
                    out.push_str(&expand_param(&body, env, last_status));
                    i = j;
                } else {
                    out.push('$');
                    i += 1;
                }
            }
            '$' => {
                let mut end = i + 1;
                if chars.get(end) == Some(&'?') {
                    end += 1;
                } else {
                    while chars.get(end).is_some_and(|c| c.is_ascii_alphanumeric() || *c == '_') {
                        end += 1;
                    }
                }
                if end == i + 1 && chars.get(end) != Some(&'?') {
                    out.push('$');
                    i += 1;
                } else {
                    let name: String = chars[i + 1..end].iter().collect();
                    out.push_str(&param_value(&name, env, last_status).unwrap_or_default());
                    i = end;
                }
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    (out, i)
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

/// One split piece of an expanded segment.
enum Piece {
    /// No delimiter before this text: it joins the field being built.
    Join(String),
    /// A delimiter closes the current field (emitting an empty one when
    /// there is none and the delimiter was non-whitespace, POSIX) and this
    /// text starts the next field.
    Next { emit_empty: bool, text: String },
}

/// Split `text` on IFS, POSIX style: runs of IFS whitespace collapse and
/// never yield empty fields; a non-whitespace IFS char (adjacent IFS
/// whitespace folds into it) terminates a field — even an empty one. Also
/// returns whether the text ends with an IFS char: a trailing delimiter
/// closes the pending field but leaves no empty field behind.
fn split_ifs(text: &str, ifs: &str) -> (Vec<Piece>, bool) {
    let is_ifs = |c: char| ifs.contains(c);
    let is_ws = |c: char| is_ifs(c) && c.is_whitespace();
    let is_nonws = |c: char| is_ifs(c) && !c.is_whitespace();
    let chars: Vec<char> = text.chars().collect();
    let mut pieces = Vec::new();
    let mut i = 0;
    let mut first = true;
    while i < chars.len() {
        // One delimiter: an IFS-whitespace run, or one non-whitespace IFS
        // char with the IFS whitespace around it.
        let mut ws_run = false;
        while i < chars.len() && is_ws(chars[i]) {
            ws_run = true;
            i += 1;
        }
        let mut hard = false;
        if i < chars.len() && is_nonws(chars[i]) {
            hard = true;
            i += 1;
            while i < chars.len() && is_ws(chars[i]) {
                i += 1;
            }
        }
        let start = i;
        while i < chars.len() && !is_ifs(chars[i]) {
            i += 1;
        }
        let content: String = chars[start..i].iter().collect();
        if first && !ws_run && !hard {
            pieces.push(Piece::Join(content));
        } else {
            pieces.push(Piece::Next { emit_empty: hard, text: content });
        }
        first = false;
    }
    let ends_with_ifs = text.chars().last().is_some_and(is_ifs);
    (pieces, ends_with_ifs)
}

/// Split segments into fields on IFS. Literal/quoted segments never split;
/// an `Expanded` segment splits per [`split_ifs`], with the leading piece
/// joining the current field (adjacent unquoted expansions behave as one
/// concatenated text, POSIX) and a trailing delimiter closing it. An empty
/// expansion contributes nothing; a word that expands to no fields at all
/// disappears entirely.
fn split_fields(segments: Vec<Seg>, ifs: &str) -> Vec<Field> {
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
                let (pieces, ends_with_ifs) = split_ifs(&text, ifs);
                for piece in pieces {
                    match piece {
                        Piece::Join(text) => {
                            let field = current.get_or_insert_with(Field::default);
                            field.has_meta |= has_glob_chars(&text);
                            field.text.push_str(&text);
                            field.pattern.push_str(&text);
                        }
                        Piece::Next { emit_empty, text } => {
                            match current.take() {
                                Some(field) => fields.push(field),
                                None if emit_empty => fields.push(Field::default()),
                                None => {}
                            }
                            current = Some(Field {
                                has_meta: has_glob_chars(&text),
                                pattern: text.clone(),
                                text,
                            });
                        }
                    }
                }
                // A trailing delimiter leaves no pending field behind.
                if ends_with_ifs {
                    current = None;
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
    // The glob crate negates classes with `!`; accept the regex-style `[^...]`
    // too. Quoted text is already escaped, so a raw `[^` is always a class.
    let pattern = field.pattern.replace("[^", "[!");
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

    #[test]
    fn braced_parameter_forms() {
        let env = env_with(&[("FOO", "bar"), ("EMPTY", "")]);
        let p = |body: &str| vec![Part::Param(body.into())];
        assert_eq!(expand_word(&p("FOO"), &env, 0), "bar");
        assert_eq!(expand_word(&p("MISSING"), &env, 0), "");
        assert_eq!(expand_word(&p("?"), &env, 7), "7");
        assert_eq!(expand_word(&p("#FOO"), &env, 0), "3");
        assert_eq!(expand_word(&p("#MISSING"), &env, 0), "0");
        // `:-` defaults on unset-or-null, `-` only on unset.
        assert_eq!(expand_word(&p("FOO:-def"), &env, 0), "bar");
        assert_eq!(expand_word(&p("MISSING:-def"), &env, 0), "def");
        assert_eq!(expand_word(&p("EMPTY:-def"), &env, 0), "def");
        assert_eq!(expand_word(&p("EMPTY-def"), &env, 0), "");
        assert_eq!(expand_word(&p("MISSING-def"), &env, 0), "def");
        // `:+`/`+` substitute the alternative when the variable is set.
        assert_eq!(expand_word(&p("FOO:+alt"), &env, 0), "alt");
        assert_eq!(expand_word(&p("EMPTY:+alt"), &env, 0), "");
        assert_eq!(expand_word(&p("EMPTY+alt"), &env, 0), "alt");
        assert_eq!(expand_word(&p("MISSING+alt"), &env, 0), "");
        // The word expands recursively: nested ${...}, $VAR, quote removal.
        let env = env_with(&[("INNER", "deep"), ("X", "a b")]);
        assert_eq!(expand_word(&p("OUTER:-${INNER}"), &env, 0), "deep");
        assert_eq!(expand_word(&p("OUTER:-$INNER/x"), &env, 0), "deep/x");
        assert_eq!(expand_word(&p("OUTER:-a \"b c\" 'd e'"), &env, 0), "a b c d e");
        // Quoted expands literally, unquoted splits like `$VAR`.
        assert_eq!(glob_word(&[Part::QParam("X".into())], &env), ["a b"]);
        assert_eq!(glob_word(&[Part::Param("X".into())], &env), ["a", "b"]);
    }

    #[test]
    fn ifs_non_whitespace_delimiters() {
        // A non-whitespace IFS char terminates a field — even an empty one.
        let env = env_with(&[("IFS", ","), ("X", "a,,b")]);
        assert_eq!(glob_word(&[Part::Var("X".into())], &env), ["a", "", "b"]);
        // A leading delimiter makes an empty field; a trailing one doesn't.
        let env = env_with(&[("IFS", ","), ("X", ",a")]);
        assert_eq!(glob_word(&[Part::Var("X".into())], &env), ["", "a"]);
        let env = env_with(&[("IFS", ","), ("X", "a,")]);
        assert_eq!(glob_word(&[Part::Var("X".into())], &env), ["a"]);
        // IFS whitespace around a non-whitespace delimiter folds into it.
        let env = env_with(&[("IFS", " ,"), ("X", "a , , b")]);
        assert_eq!(glob_word(&[Part::Var("X".into())], &env), ["a", "", "b"]);
        // A leading delimiter terminates a literal prefix, without an extra
        // empty field.
        let env = env_with(&[("IFS", ","), ("X", ",a")]);
        assert_eq!(
            glob_word(&[Part::Lit("pre".into()), Part::Var("X".into())], &env),
            ["pre", "a"]
        );
        // Adjacent unquoted expansions split as one concatenated text.
        let env = env_with(&[("IFS", ","), ("X", "a,"), ("Y", ",b")]);
        assert_eq!(
            glob_word(&[Part::Var("X".into()), Part::Var("Y".into())], &env),
            ["a", "", "b"]
        );
        // A trailing delimiter closes the field: the next segment starts
        // fresh instead of joining.
        let env = env_with(&[("IFS", ","), ("X", "a,")]);
        assert_eq!(
            glob_word(&[Part::Var("X".into()), Part::Lit("post".into())], &env),
            ["a", "post"]
        );
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
        // Negation is `[!...]` in the glob crate; regex-style `[^...]` is
        // accepted too.
        assert_eq!(expand_in(&dir, "[!a]*.rs"), vec!["b.rs"]);
        assert_eq!(expand_in(&dir, "[^a]*.rs"), vec!["b.rs"]);
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
