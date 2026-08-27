//! Tab completion: command names in command position, file paths elsewhere.
//!
//! The pure part (`position`, `candidates`, `complete`) works on injected
//! candidate lists so tests don't depend on the real PATH or filesystem;
//! main.rs feeds it builtins + PATH executables and delegates argument
//! completion to rustyline's `FilenameCompleter`.

use crate::lexer::{self, Token};

/// Whether the word under the cursor is a command name or an argument.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Position {
    Command,
    Argument,
}

/// Classify `pos` (a char index into `line`) with the same command-position
/// logic as the highlighter: the first word after the start, `|`, `;`, `&&`
/// or `||` is a command, everything else an argument. On a lexer error (e.g.
/// an open quote while typing) fall back to argument position — path
/// completion still works there.
pub fn position(line: &str, pos: usize) -> Position {
    let Ok(spans) = lexer::lex_with_spans(line) else {
        return Position::Argument;
    };
    let mut expect_command = true;
    for (span, token) in &spans {
        if span.start >= pos {
            break; // the cursor is before this token: a fresh word
        }
        match token {
            Token::Pipe | Token::Semi | Token::And | Token::Or | Token::Amp => {
                expect_command = true;
            }
            Token::Out { .. } | Token::ErrOut { .. } | Token::In => {}
            Token::Word(_) => {
                // Inside (or right after) this word: its kind decides.
                if pos <= span.end {
                    return if expect_command { Position::Command } else { Position::Argument };
                }
                expect_command = false;
            }
        }
    }
    if expect_command { Position::Command } else { Position::Argument }
}

/// Char index where the word under `pos` starts (word chars are anything but
/// whitespace and shell operators).
pub fn word_start(line: &str, pos: usize) -> usize {
    let chars: Vec<char> = line.chars().collect();
    let mut start = pos.min(chars.len());
    while start > 0 {
        let c = chars[start - 1];
        if c.is_whitespace() || matches!(c, '|' | ';' | '&' | '>' | '<') {
            break;
        }
        start -= 1;
    }
    start
}

/// Entries of `choices` starting with `word`, sorted and deduped.
pub fn candidates(word: &str, choices: &[String]) -> Vec<String> {
    let mut matches: Vec<String> =
        choices.iter().filter(|c| c.starts_with(word)).cloned().collect();
    matches.sort();
    matches.dedup();
    matches
}

/// Complete the word under `pos` (char index): from `commands` in command
/// position, from `files` in argument position. Returns the word's start and
/// the candidates.
pub fn complete(
    line: &str,
    pos: usize,
    commands: &[String],
    files: &[String],
) -> (usize, Vec<String>) {
    let start = word_start(line, pos);
    let word: String = line.chars().skip(start).take(pos.saturating_sub(start)).collect();
    let choices = match position(line, pos) {
        Position::Command => commands,
        Position::Argument => files,
    };
    (start, candidates(&word, choices))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| w.to_string()).collect()
    }

    #[test]
    fn command_and_argument_positions() {
        assert_eq!(position("", 0), Position::Command);
        assert_eq!(position("ec", 2), Position::Command);
        assert_eq!(position("echo hi", 7), Position::Argument);
        assert_eq!(position("echo hi", 4), Position::Command); // completing `echo` itself
        assert_eq!(position("echo ", 5), Position::Argument); // fresh word after a space
        assert_eq!(position("echo a | l", 10), Position::Command);
        assert_eq!(position("echo a && l", 11), Position::Command);
        assert_eq!(position("echo a ; l", 10), Position::Command);
        assert_eq!(position("cat > out", 9), Position::Argument); // redirect target
    }

    #[test]
    fn lexer_error_falls_back_to_paths() {
        assert_eq!(position("echo 'unterminated", 18), Position::Argument);
        assert_eq!(position("'unterminated", 13), Position::Argument);
    }

    #[test]
    fn complete_command_position_uses_command_list() {
        let commands = strings(&["echo", "exit", "env", "ls"]);
        let files = strings(&["earnings.txt"]);
        let (start, found) = complete("e", 1, &commands, &files);
        assert_eq!(start, 0);
        assert_eq!(found, strings(&["echo", "env", "exit"]));
        // After an operator the command list applies again.
        let (start, found) = complete("ls | ex", 7, &commands, &files);
        assert_eq!(start, 5);
        assert_eq!(found, strings(&["exit"]));
    }

    #[test]
    fn complete_argument_position_uses_file_list() {
        let commands = strings(&["echo"]);
        let files = strings(&["src", "src.rs", "target"]);
        let (start, found) = complete("cat s", 5, &commands, &files);
        assert_eq!(start, 4);
        assert_eq!(found, strings(&["src", "src.rs"]));
    }

    #[test]
    fn word_start_stops_at_operators_and_space() {
        assert_eq!(word_start("echo hi", 7), 5);
        assert_eq!(word_start("ls|ca", 5), 3);
        assert_eq!(word_start("ls>o", 4), 3);
        assert_eq!(word_start("", 0), 0);
    }
}
