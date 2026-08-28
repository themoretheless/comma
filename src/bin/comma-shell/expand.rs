//! Word expansion: `{a,b}`/`{1..10}` brace groups (first, bash-style, only
//! in unquoted literal text), then `$VAR`, `${...}` forms (incl.
//! `${VAR#pat}`/`${VAR%pat}` and nested words), `$((...))` arithmetic, `$?`,
//! leading `~`, `$(...)` and `` `...` `` output, word splitting and globbing
//! (`*`, `?`, `[...]` with `[!]`/`[^]` negation, `**` recursively), in POSIX
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
            Part::Arith(body) => segments.push(Seg::Expanded(eval_arith(body, env))),
            Part::QArith(body) => segments.push(Seg::Quoted(eval_arith(body, env))),
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
        // `${VAR#pat}`/`${VAR##pat}` strip a prefix, `${VAR%pat}`/
        // `${VAR%%pat}` a suffix.
        '#' | '%' if !test_null => {
            let (longest, word) = match word.strip_prefix(op) {
                Some(word) => (true, word),
                None => (false, word),
            };
            let pattern = expand_braced_word(word, env, last_status);
            trim_pattern(&val.unwrap_or_default(), &pattern, op == '#', longest)
        }
        _ => String::new(),
    }
}

/// `${VAR#word}`/`${VAR%word}`: strip the shortest (or longest, with `##` /
/// `%%`) prefix/suffix matching the glob pattern `word`; no match leaves the
/// value unchanged.
fn trim_pattern(value: &str, pattern: &str, prefix: bool, longest: bool) -> String {
    let Ok(pattern) = glob::Pattern::new(pattern) else {
        return value.to_string();
    };
    // Patterns here match a string, not a path: `/` and dots are ordinary.
    let options = glob::MatchOptions {
        case_sensitive: true,
        require_literal_separator: false,
        require_literal_leading_dot: false,
    };
    // Char-boundary offsets, shortest match first for the tested side.
    let bounds: Vec<usize> =
        value.char_indices().map(|(i, _)| i).chain(std::iter::once(value.len())).collect();
    let ascending = prefix != longest;
    let order: Box<dyn Iterator<Item = &usize>> = if ascending {
        Box::new(bounds.iter())
    } else {
        Box::new(bounds.iter().rev())
    };
    for &len in order {
        let (head, tail) = value.split_at(len);
        if pattern.matches_with(if prefix { head } else { tail }, options) {
            return (if prefix { tail } else { head }).to_string();
        }
    }
    value.to_string()
}

/// Evaluate a `$((...))` body: i64 arithmetic with `+ - * / %`, unary
/// `-`/`+`, parentheses and variables (bare or `$name`; unset or
/// non-numeric variables count as 0). Any error — syntax, division by
/// zero, overflow — yields an empty expansion.
fn eval_arith(body: &str, env: &HashMap<String, String>) -> String {
    let chars: Vec<char> = body.chars().collect();
    let mut parser = ArithParser { chars: &chars, pos: 0, env };
    match parser.expr().filter(|_| parser.at_end()) {
        Some(value) => value.to_string(),
        None => String::new(),
    }
}

struct ArithParser<'a> {
    chars: &'a [char],
    pos: usize,
    env: &'a HashMap<String, String>,
}

impl ArithParser<'_> {
    fn skip_ws(&mut self) {
        while self.chars.get(self.pos).is_some_and(|c| c.is_whitespace()) {
            self.pos += 1;
        }
    }

    fn at_end(&mut self) -> bool {
        self.skip_ws();
        self.pos == self.chars.len()
    }

    fn eat(&mut self, c: char) -> bool {
        self.skip_ws();
        if self.chars.get(self.pos) == Some(&c) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn expr(&mut self) -> Option<i64> {
        let mut lhs = self.term()?;
        loop {
            if self.eat('+') {
                lhs = lhs.checked_add(self.term()?)?;
            } else if self.eat('-') {
                lhs = lhs.checked_sub(self.term()?)?;
            } else {
                return Some(lhs);
            }
        }
    }

    fn term(&mut self) -> Option<i64> {
        let mut lhs = self.factor()?;
        loop {
            if self.eat('*') {
                lhs = lhs.checked_mul(self.factor()?)?;
            } else if self.eat('/') {
                lhs = lhs.checked_div(self.factor()?)?;
            } else if self.eat('%') {
                lhs = lhs.checked_rem(self.factor()?)?;
            } else {
                return Some(lhs);
            }
        }
    }

    fn factor(&mut self) -> Option<i64> {
        if self.eat('-') {
            return self.factor()?.checked_neg();
        }
        if self.eat('+') {
            return self.factor();
        }
        if self.eat('(') {
            let value = self.expr()?;
            return self.eat(')').then_some(value);
        }
        self.number().or_else(|| self.variable())
    }

    fn number(&mut self) -> Option<i64> {
        self.skip_ws();
        let start = self.pos;
        while self.chars.get(self.pos).is_some_and(|c| c.is_ascii_digit()) {
            self.pos += 1;
        }
        if start == self.pos {
            return None;
        }
        self.chars[start..self.pos].iter().collect::<String>().parse().ok()
    }

    fn variable(&mut self) -> Option<i64> {
        self.skip_ws();
        let mut pos = self.pos;
        if self.chars.get(pos) == Some(&'$') {
            pos += 1;
        }
        let start = pos;
        while self.chars.get(pos).is_some_and(|c| c.is_ascii_alphanumeric() || *c == '_') {
            pos += 1;
        }
        if pos == start {
            return None;
        }
        let name: String = self.chars[start..pos].iter().collect();
        self.pos = pos;
        Some(self.env.get(&name).and_then(|v| v.trim().parse().ok()).unwrap_or(0))
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

/// Expand one word into argv entries: brace expansion (bash-style, first),
/// then per resulting word variable/tilde expansion, word splitting and
/// globbing.
fn expand_glob_word(
    parts: &[Part],
    env: &HashMap<String, String>,
    last_status: i32,
) -> Vec<String> {
    // An explicitly quoted empty string still yields one (empty) field.
    if parts.is_empty() {
        return vec![String::new()];
    }
    let ifs = ifs_chars(env);
    let mut out = Vec::new();
    for variant in brace_expand_parts(parts) {
        let fields = split_fields(expand_segments(&variant, env, last_status), &ifs);
        out.extend(fields.iter().flat_map(glob_field));
    }
    out
}

/// Multiply a word over the brace groups of its unquoted literal parts
/// (cartesian product when several parts/groups expand); every other part
/// is copied to every variant unchanged, so quoting and `$...` expansions
/// never participate.
fn brace_expand_parts(parts: &[Part]) -> Vec<Vec<Part>> {
    let mut variants: Vec<Vec<Part>> = vec![Vec::new()];
    for part in parts {
        match part {
            Part::Lit(text) => {
                let alts = brace_expand(text);
                let mut next = Vec::with_capacity(variants.len() * alts.len());
                for variant in &variants {
                    for alt in &alts {
                        let mut variant = variant.clone();
                        variant.push(Part::Lit(alt.clone()));
                        next.push(variant);
                    }
                }
                variants = next;
            }
            other => {
                for variant in &mut variants {
                    variant.push(other.clone());
                }
            }
        }
    }
    variants
}

/// Brace-expand one literal text: `{a,b}` comma lists and `{1..10[..step]}`
/// integer ranges (descending and negative ranges work), nesting included.
/// The leftmost valid group expands first; a group with no closing `}`, no
/// top-level comma and no valid range stays literal, and scanning continues
/// after it.
fn brace_expand(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    for (i, &c) in chars.iter().enumerate() {
        if c != '{' {
            continue;
        }
        if let Some((alts, end)) = parse_brace_group(&chars, i) {
            let prefix: String = chars[..i].iter().collect();
            let suffix: String = chars[end..].iter().collect();
            let mut out = Vec::new();
            for alt in alts {
                for rest in brace_expand(&suffix) {
                    out.push(format!("{prefix}{alt}{rest}"));
                }
            }
            return out;
        }
    }
    vec![text.to_string()]
}

/// Parse the `{...}` group at `chars[start]`, returning the fully expanded
/// alternatives and the index just past the closing `}`; `None` when the
/// group is invalid and must stay literal.
fn parse_brace_group(chars: &[char], start: usize) -> Option<(Vec<String>, usize)> {
    // Split the body on top-level commas; braces nest.
    let mut depth = 0;
    let mut parts: Vec<Vec<char>> = vec![Vec::new()];
    let mut end = None;
    for (i, &c) in chars.iter().enumerate().skip(start + 1) {
        match c {
            '{' => {
                depth += 1;
                parts.last_mut()?.push(c);
            }
            '}' if depth == 0 => {
                end = Some(i + 1);
                break;
            }
            '}' => {
                depth -= 1;
                parts.last_mut()?.push(c);
            }
            ',' if depth == 0 => parts.push(Vec::new()),
            _ => parts.last_mut()?.push(c),
        }
    }
    let end = end?;
    if parts.len() > 1 {
        // Comma list: every alternative brace-expands recursively.
        let alts = parts
            .iter()
            .flat_map(|part| brace_expand(&part.iter().collect::<String>()))
            .collect();
        return Some((alts, end));
    }
    // No top-level comma: only an integer range is a valid group.
    let body: String = parts[0].iter().collect();
    range_expansion(&body).map(|alts| (alts, end))
}

/// `{a..b[..step]}` integer range, ascending or descending; the step is a
/// magnitude. `None` when the body is not a valid range (the group then
/// stays literal).
fn range_expansion(body: &str) -> Option<Vec<String>> {
    let mut pieces = body.split("..");
    let first: i64 = pieces.next()?.parse().ok()?;
    let last: i64 = pieces.next()?.parse().ok()?;
    let step: i64 = match pieces.next() {
        Some(piece) => piece.parse().ok()?,
        None => 1,
    };
    if pieces.next().is_some() || step == 0 {
        return None;
    }
    let step = step.unsigned_abs();
    let mut out = Vec::new();
    let mut n = first;
    if first <= last {
        while n <= last {
            out.push(n.to_string());
            n = n.checked_add(step as i64)?;
        }
    } else {
        while n >= last {
            out.push(n.to_string());
            n = n.checked_sub(step as i64)?;
        }
    }
    Some(out)
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

    #[test]
    fn arithmetic() {
        let env = env_with(&[("X", "5")]);
        let a = |body: &str| vec![Part::Arith(body.into())];
        assert_eq!(expand_word(&a("1 + 2 * 3"), &env, 0), "7");
        assert_eq!(expand_word(&a("(1 + 2) * 3"), &env, 0), "9");
        // Bare and `$`-prefixed variables; unset counts as 0.
        assert_eq!(expand_word(&a("X * 2"), &env, 0), "10");
        assert_eq!(expand_word(&a("$X + MISSING"), &env, 0), "5");
        assert_eq!(expand_word(&a("-X"), &env, 0), "-5");
        assert_eq!(expand_word(&a("10 % 3"), &env, 0), "1");
        assert_eq!(expand_word(&a("10 / 3"), &env, 0), "3");
        // Errors expand to nothing: division by zero, trailing operator.
        assert_eq!(expand_word(&a("1 / 0"), &env, 0), "");
        assert_eq!(expand_word(&a("1 +"), &env, 0), "");
    }

    #[test]
    fn pattern_removal_forms() {
        let env = env_with(&[("V", "path/to/file.txt")]);
        let p = |body: &str| vec![Part::Param(body.into())];
        assert_eq!(expand_word(&p("V#*/"), &env, 0), "to/file.txt");
        assert_eq!(expand_word(&p("V##*/"), &env, 0), "file.txt");
        assert_eq!(expand_word(&p("V%/*"), &env, 0), "path/to");
        assert_eq!(expand_word(&p("V%%/*"), &env, 0), "path");
        // No match: the value stays; the pattern word expands.
        assert_eq!(expand_word(&p("V#z*"), &env, 0), "path/to/file.txt");
        let env = env_with(&[("V", "file.tar.gz"), ("E", "*.gz")]);
        assert_eq!(expand_word(&p("V%$E"), &env, 0), "file.tar");
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

    /// Serializes tests that change the process cwd (shared with the exec
    /// test module, same test process).
    use crate::CWD_LOCK;

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

    /// Expand one lexed word (no cwd needed: no globbing involved).
    fn brace_word(line: &str) -> Vec<String> {
        let env = env_with(&[("FOO", "bar")]);
        let tokens = crate::lexer::lex(line).unwrap();
        match &tokens[0] {
            crate::lexer::Token::Word(parts) => expand_glob_word(parts, &env, 0),
            other => panic!("expected a word, got {other:?}"),
        }
    }

    #[test]
    fn brace_comma_lists_and_cartesian_products() {
        assert_eq!(brace_word("{a,b}"), ["a", "b"]);
        // Prefix and suffix join every alternative.
        assert_eq!(brace_word("a{b,c}d"), ["abd", "acd"]);
        // Several groups form a cartesian product, left to right.
        assert_eq!(brace_word("{a,b}{c,d}"), ["ac", "ad", "bc", "bd"]);
        assert_eq!(brace_word("x{a,b}y{c,d}z"), ["xaycz", "xaydz", "xbycz", "xbydz"]);
        // An empty alternative is allowed.
        assert_eq!(brace_word("{,a}.txt"), [".txt", "a.txt"]);
        // Nested groups expand innermost-out, order preserved.
        assert_eq!(brace_word("x{a,{b,c}}y"), ["xay", "xby", "xcy"]);
        assert_eq!(brace_word("{{a,b},c}"), ["a", "b", "c"]);
        assert_eq!(brace_word("{a,{b,{c,d}}}"), ["a", "b", "c", "d"]);
    }

    #[test]
    fn brace_integer_ranges() {
        assert_eq!(brace_word("{1..5}"), ["1", "2", "3", "4", "5"]);
        // Descending range.
        assert_eq!(brace_word("{3..1}"), ["3", "2", "1"]);
        // Explicit step, both directions.
        assert_eq!(brace_word("{0..12..3}"), ["0", "3", "6", "9", "12"]);
        assert_eq!(brace_word("{5..1..2}"), ["5", "3", "1"]);
        // Negative bounds.
        assert_eq!(brace_word("{-2..2}"), ["-2", "-1", "0", "1", "2"]);
        // Range with affixes.
        assert_eq!(brace_word("f{1..3}.rs"), ["f1.rs", "f2.rs", "f3.rs"]);
    }

    #[test]
    fn invalid_brace_groups_stay_literal() {
        // No comma and not a range.
        assert_eq!(brace_word("{a}"), ["{a}"]);
        assert_eq!(brace_word("{}"), ["{}"]);
        // No closing brace.
        assert_eq!(brace_word("{a,b"), ["{a,b"]);
        // Not an integer range.
        assert_eq!(brace_word("{a..b}"), ["{a..b}"]);
        assert_eq!(brace_word("{1..}"), ["{1..}"]);
        assert_eq!(brace_word("{1..2..0}"), ["{1..2..0}"]);
        // An invalid outer group does not block a valid inner one.
        assert_eq!(brace_word("a{b{c,d}e"), ["a{bce", "a{bde"]);
    }

    #[test]
    fn brace_expansion_skips_quotes_and_expansions() {
        // Quoted braces never expand.
        assert_eq!(brace_word("'{a,b}'"), ["{a,b}"]);
        assert_eq!(brace_word("\"{1..3}\""), ["{1..3}"]);
        // A variable holding braces is not re-scanned.
        let env = env_with(&[("B", "{a,b}")]);
        assert_eq!(glob_word(&[Part::Var("B".into())], &env), ["{a,b}"]);
        // ...but braces around a variable still multiply the word.
        assert_eq!(brace_word("x{1,2}-$FOO"), ["x1-bar", "x2-bar"]);
        // Quoting is resolved at lex time: quotes inside a brace group split
        // it across parts, so the group does not expand (architectural
        // limitation; bash would).
        assert_eq!(brace_word("{'a b',c}"), ["{a b,c}"]);
    }

    #[test]
    fn brace_results_still_glob() {
        let _guard = CWD_LOCK.lock().unwrap();
        let dir = glob_dir("brace");
        assert_eq!(expand_in(&dir, "*.{rs,txt}"), vec!["a.rs", "b.rs", "c.txt"]);
        assert_eq!(expand_in(&dir, "{a,b}.rs"), vec!["a.rs", "b.rs"]);
        std::fs::remove_dir_all(&dir).ok();
    }
}
