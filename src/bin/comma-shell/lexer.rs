//! Tokenizer: splits input into words (with quoting info) and operators.

/// Lexer/parser error with a user-facing message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    /// A quote was never closed; holds the quote character.
    UnterminatedQuote(char),
    /// Input ends with a dangling `\`.
    TrailingBackslash,
    /// `&` not followed by another `&` or placed where it cannot background
    /// a pipeline.
    UnexpectedAmp,
    /// An operator appeared where a command was expected.
    ExpectedCommand,
    /// A redirect operator lacks its target word.
    MissingRedirectTarget,
    /// A `$(` was never closed.
    UnterminatedSubst,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            ParseError::UnterminatedQuote('\'') => "unterminated single quote",
            ParseError::UnterminatedQuote('"') => "unterminated double quote",
            ParseError::UnterminatedQuote(_) => "unterminated quote",
            ParseError::TrailingBackslash => "trailing backslash",
            ParseError::UnexpectedAmp => "unexpected '&'",
            ParseError::ExpectedCommand => "expected a command",
            ParseError::MissingRedirectTarget => "missing redirect target",
            ParseError::UnterminatedSubst => "unterminated command substitution",
        };
        f.write_str(message)
    }
}

impl std::error::Error for ParseError {}

/// Part of a word. Quoting is resolved at lex time; expansion happens later.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Part {
    /// Literal unquoted text (glob metacharacters stay live).
    Lit(String),
    /// Literal quoted text (no globbing, no word splitting).
    QLit(String),
    /// Unquoted environment variable reference (`$NAME` or `$?`); the value
    /// is word-split and globbed (POSIX).
    Var(String),
    /// Double-quoted variable reference: expands literally.
    QVar(String),
    /// Leading unquoted `~`.
    Tilde,
    /// Unquoted `$(...)` command substitution; holds the raw inner command
    /// line. The output is word-split and globbed.
    Subst(String),
    /// Double-quoted `$(...)`; the output is substituted literally.
    QSubst(String),
    /// Output of an already-executed unquoted substitution (executor fills
    /// this in); word-split and globbed like a variable.
    SubstOut(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Token {
    Word(Vec<Part>),
    Pipe,
    Semi,
    And,
    Or,
    /// `&` — backgrounds the preceding pipeline (parser validates placement).
    Amp,
    /// `>` or `>>`.
    Out { append: bool },
    /// `2>` or `2>>`.
    ErrOut { append: bool },
    /// `<`.
    In,
}

pub fn lex(input: &str) -> Result<Vec<Token>, ParseError> {
    Ok(lex_with_spans(input)?.into_iter().map(|(_, token)| token).collect())
}

/// Like [`lex`], but each token carries its char-index span in the input
/// (used by the line highlighter).
pub fn lex_with_spans(
    input: &str,
) -> Result<Vec<(std::ops::Range<usize>, Token)>, ParseError> {
    let chars: Vec<char> = input.chars().collect();
    let mut tokens = Vec::new();
    let mut i = 0;
    macro_rules! push {
        ($start:expr, $token:expr) => {
            tokens.push(($start..i, $token))
        };
    }
    while i < chars.len() {
        let start = i;
        match chars[i] {
            c if c.is_whitespace() => i += 1,
            '|' => {
                if chars.get(i + 1) == Some(&'|') {
                    i += 2;
                    push!(start, Token::Or);
                } else {
                    i += 1;
                    push!(start, Token::Pipe);
                }
            }
            ';' => {
                i += 1;
                push!(start, Token::Semi);
            }
            '&' => {
                if chars.get(i + 1) == Some(&'&') {
                    i += 2;
                    push!(start, Token::And);
                } else {
                    i += 1;
                    push!(start, Token::Amp);
                }
            }
            '>' => {
                if chars.get(i + 1) == Some(&'>') {
                    i += 2;
                    push!(start, Token::Out { append: true });
                } else {
                    i += 1;
                    push!(start, Token::Out { append: false });
                }
            }
            '<' => {
                i += 1;
                push!(start, Token::In);
            }
            '2' if chars.get(i + 1) == Some(&'>') => {
                if chars.get(i + 2) == Some(&'>') {
                    i += 3;
                    push!(start, Token::ErrOut { append: true });
                } else {
                    i += 2;
                    push!(start, Token::ErrOut { append: false });
                }
            }
            _ => {
                let (parts, next) = lex_word(&chars, i)?;
                i = next;
                push!(start, Token::Word(parts));
            }
        }
    }
    Ok(tokens)
}

fn is_operator(c: char) -> bool {
    matches!(c, '|' | ';' | '&' | '>' | '<')
}

fn lex_word(chars: &[char], mut i: usize) -> Result<(Vec<Part>, usize), ParseError> {
    let mut parts = Vec::new();
    let mut lit = String::new(); // unquoted literal run
    let mut quoted: Option<String> = None; // quoted literal run
    let word_start = i;

    macro_rules! flush_lit {
        () => {
            if !lit.is_empty() {
                parts.push(Part::Lit(std::mem::take(&mut lit)));
            }
        };
    }
    macro_rules! flush_quoted {
        () => {
            if let Some(text) = quoted.take()
                && !text.is_empty()
            {
                parts.push(Part::QLit(text));
            }
        };
    }
    macro_rules! flush_all {
        () => {
            flush_lit!();
            flush_quoted!();
        };
    }
    macro_rules! push_unquoted {
        ($c:expr) => {{
            flush_quoted!();
            lit.push($c);
        }};
    }
    macro_rules! push_quoted {
        ($c:expr) => {{
            flush_lit!();
            quoted.get_or_insert_with(String::new).push($c);
        }};
    }

    while i < chars.len() {
        let c = chars[i];
        match c {
            c if c.is_whitespace() || is_operator(c) => break,
            '\\' => {
                i += 1;
                match chars.get(i) {
                    Some(&c) => push_unquoted!(c),
                    None => return Err(ParseError::TrailingBackslash),
                }
                i += 1;
            }
            '\'' => {
                i += 1;
                loop {
                    match chars.get(i) {
                        None => return Err(ParseError::UnterminatedQuote('\'')),
                        Some(&'\'') => {
                            i += 1;
                            break;
                        }
                        Some(&c) => {
                            push_quoted!(c);
                            i += 1;
                        }
                    }
                }
            }
            '"' => {
                i += 1;
                loop {
                    match chars.get(i) {
                        None => return Err(ParseError::UnterminatedQuote('"')),
                        Some(&'"') => {
                            i += 1;
                            break;
                        }
                        Some(&'\\') => {
                            i += 1;
                            match chars.get(i) {
                                Some(&c @ ('\\' | '"' | '$' | '`')) => {
                                    push_quoted!(c);
                                    i += 1;
                                }
                                Some(_) => {
                                    push_quoted!('\\');
                                }
                                None => return Err(ParseError::UnterminatedQuote('"')),
                            }
                        }
                        Some(&'$') if chars.get(i + 1) == Some(&'(') => {
                            flush_all!();
                            let (body, next) = lex_subst(chars, i + 1)?;
                            parts.push(Part::QSubst(body));
                            i = next;
                        }
                        Some(&'$') => {
                            flush_all!();
                            i += 1;
                            let (name, next) = lex_var(chars, i);
                            match name {
                                Some(name) => parts.push(Part::QVar(name)),
                                None => push_quoted!('$'),
                            }
                            i = next;
                        }
                        Some(&c) => {
                            push_quoted!(c);
                            i += 1;
                        }
                    }
                }
            }
            '$' if chars.get(i + 1) == Some(&'(') => {
                flush_all!();
                let (body, next) = lex_subst(chars, i + 1)?;
                parts.push(Part::Subst(body));
                i = next;
            }
            '$' => {
                flush_all!();
                i += 1;
                let (name, next) = lex_var(chars, i);
                match name {
                    Some(name) => parts.push(Part::Var(name)),
                    None => push_unquoted!('$'),
                }
                i = next;
            }
            '~' if i == word_start => {
                flush_all!();
                parts.push(Part::Tilde);
                i += 1;
            }
            _ => {
                push_unquoted!(c);
                i += 1;
            }
        }
    }
    flush_all!();
    Ok((parts, i))
}

/// Body of a `$(...)` substitution; `start` points at the `(` after `$`.
/// Returns the raw inner command line and the index just past the closing
/// `)`. Parentheses must balance (so nested `$(...)` works); quoted spans
/// inside are copied verbatim and don't count.
fn lex_subst(chars: &[char], start: usize) -> Result<(String, usize), ParseError> {
    let mut depth = 0;
    let mut body = String::new();
    let mut i = start;
    while let Some(&c) = chars.get(i) {
        match c {
            '(' => {
                depth += 1;
                if depth > 1 {
                    body.push(c);
                }
                i += 1;
            }
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Ok((body, i + 1));
                }
                body.push(c);
                i += 1;
            }
            quote @ ('\'' | '"') => {
                body.push(quote);
                i += 1;
                loop {
                    match chars.get(i) {
                        None => return Err(ParseError::UnterminatedQuote(quote)),
                        Some(&c) => {
                            body.push(c);
                            i += 1;
                            if c == quote {
                                break;
                            }
                        }
                    }
                }
            }
            _ => {
                body.push(c);
                i += 1;
            }
        }
    }
    Err(ParseError::UnterminatedSubst)
}

/// Variable name after `$`: `[A-Za-z_][A-Za-z0-9_]*` or `?`.
fn lex_var(chars: &[char], start: usize) -> (Option<String>, usize) {
    if chars.get(start) == Some(&'?') {
        return (Some("?".into()), start + 1);
    }
    let mut end = start;
    while let Some(&c) = chars.get(end) {
        if c.is_ascii_alphanumeric() || c == '_' {
            end += 1;
        } else {
            break;
        }
    }
    if end == start { (None, start) } else { (Some(chars[start..end].iter().collect()), end) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn word(input: &str) -> Vec<Part> {
        match lex(input).unwrap().as_slice() {
            [Token::Word(parts)] => parts.clone(),
            other => panic!("expected single word, got {other:?}"),
        }
    }

    #[test]
    fn operators() {
        let tokens = lex("a | b ; c && d || e > f >> g < h 2> i 2>> j").unwrap();
        let ops: Vec<Token> = tokens.into_iter().filter(|t| !matches!(t, Token::Word(_))).collect();
        assert_eq!(
            ops,
            vec![
                Token::Pipe,
                Token::Semi,
                Token::And,
                Token::Or,
                Token::Out { append: false },
                Token::Out { append: true },
                Token::In,
                Token::ErrOut { append: false },
                Token::ErrOut { append: true },
            ]
        );
    }

    #[test]
    fn quoting() {
        assert_eq!(word("'a $b'"), vec![Part::QLit("a $b".into())]);
        assert_eq!(
            word("\"a $b c\""),
            vec![Part::QLit("a ".into()), Part::QVar("b".into()), Part::QLit(" c".into())]
        );
        assert_eq!(word("a'b'c"), vec![Part::Lit("a".into()), Part::QLit("b".into()), Part::Lit("c".into())]);
        assert_eq!(word("''"), vec![]);
    }

    #[test]
    fn command_substitution() {
        assert_eq!(word("$(echo hi)"), vec![Part::Subst("echo hi".into())]);
        assert_eq!(
            word("\"pre-$(echo hi)\""),
            vec![Part::QLit("pre-".into()), Part::QSubst("echo hi".into())]
        );
        // Nested substitutions and quotes balance.
        assert_eq!(word("$(echo $(echo x))"), vec![Part::Subst("echo $(echo x)".into())]);
        assert_eq!(word("$(echo '(')"), vec![Part::Subst("echo '('".into())]);
        // Single quotes shield the substitution.
        assert_eq!(word("'$(x)'"), vec![Part::QLit("$(x)".into())]);
        assert_eq!(lex("$(echo hi"), Err(ParseError::UnterminatedSubst));
    }

    #[test]
    fn variables_and_tilde() {
        assert_eq!(word("$HOME/x"), vec![Part::Var("HOME".into()), Part::Lit("/x".into())]);
        assert_eq!(word("$?"), vec![Part::Var("?".into())]);
        assert_eq!(word("~/docs"), vec![Part::Tilde, Part::Lit("/docs".into())]);
        assert_eq!(word("a~"), vec![Part::Lit("a~".into())]);
        assert_eq!(word("$"), vec![Part::Lit("$".into())]);
    }

    #[test]
    fn escapes() {
        assert_eq!(word("a\\ b"), vec![Part::Lit("a b".into())]);
        assert_eq!(word("\"a\\\"b\""), vec![Part::QLit("a\"b".into())]);
    }

    #[test]
    fn errors() {
        assert!(lex("'abc").is_err());
        assert!(lex("\"abc").is_err());
    }

    #[test]
    fn single_amp_is_a_token() {
        // `&` lexes fine; placement is validated by the parser.
        let tokens = lex("a &").unwrap();
        assert!(matches!(&tokens[1], Token::Amp));
    }

    #[test]
    fn digit_is_not_always_stderr_redirect() {
        // `2` alone is a word; `2>` is a stderr redirect.
        let tokens = lex("echo 2 > f").unwrap();
        assert!(matches!(&tokens[1], Token::Word(_)));
    }
}

#[cfg(test)]
mod span_tests {
    use super::*;

    #[test]
    fn spans_cover_tokens_in_order() {
        let input = "ab | 'c d' > e";
        let chars: Vec<char> = input.chars().collect();
        let spans = lex_with_spans(input).unwrap();
        let texts: Vec<String> =
            spans.iter().map(|(span, _)| chars[span.clone()].iter().collect()).collect();
        assert_eq!(texts, vec!["ab", "|", "'c d'", ">", "e"]);
        assert!(matches!(spans[0].1, Token::Word(_)));
        assert!(matches!(spans[1].1, Token::Pipe));
        assert!(matches!(spans[3].1, Token::Out { append: false }));
    }
}
