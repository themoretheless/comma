//! Command blocks from OSC 133 semantic-prompt marks.
//!
//! The PTY reader splits the output stream with `MarkScanner`, feeding plain
//! bytes to the terminal parser and recording each mark in `Blocks` at the
//! cursor's absolute line (scrollback lines + screen line).

/// One semantic-prompt mark.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mark {
    /// `A`: a prompt starts.
    Prompt,
    /// `C`: the command's output starts.
    Output,
    /// `D;status`: the command finished.
    End(Option<i32>),
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Segment {
    Bytes(Vec<u8>),
    Mark(Mark),
}

const INTRO: &[u8] = b"\x1b]133;";
/// Longest mark body we wait for before giving up and passing bytes through.
const MAX_PENDING: usize = 64;

/// Splits a byte stream at OSC 133 sequences, also when a sequence is cut
/// across reads.
#[derive(Default)]
pub(crate) struct MarkScanner {
    pending: Vec<u8>,
}

impl MarkScanner {
    pub(crate) fn feed(&mut self, input: &[u8]) -> Vec<Segment> {
        let mut data = std::mem::take(&mut self.pending);
        data.extend_from_slice(input);
        let mut out = Vec::new();
        let mut plain = 0; // start of not-yet-emitted plain bytes
        let mut i = 0;
        while i < data.len() {
            if data[i] != 0x1b {
                i += 1;
                continue;
            }
            let rest = &data[i..];
            if rest.len() < INTRO.len() {
                if INTRO.starts_with(rest) {
                    break; // maybe the start of a mark: wait for more
                }
                i += 1;
                continue;
            }
            if !rest.starts_with(INTRO) {
                i += 1;
                continue;
            }
            let body_start = i + INTRO.len();
            let terminator = data[body_start..].iter().enumerate().find_map(|(k, &b)| match b {
                0x07 => Some((k, 1)),
                0x1b if data.get(body_start + k + 1) == Some(&b'\\') => Some((k, 2)),
                _ => None,
            });
            match terminator {
                Some((len, term_len)) => {
                    if plain < i {
                        out.push(Segment::Bytes(data[plain..i].to_vec()));
                    }
                    if let Some(mark) = parse(&data[body_start..body_start + len]) {
                        out.push(Segment::Mark(mark));
                    }
                    i = body_start + len + term_len;
                    plain = i;
                }
                None if data.len() - i <= MAX_PENDING => break,
                None => i += 1,
            }
        }
        // Hold back a possibly incomplete mark at the end.
        let held = if i < data.len() { i } else { data.len() };
        if plain < held {
            out.push(Segment::Bytes(data[plain..held].to_vec()));
        }
        self.pending = data[held.max(plain)..].to_vec();
        out
    }
}

fn parse(body: &[u8]) -> Option<Mark> {
    let body = std::str::from_utf8(body).ok()?;
    let mut parts = body.split(';');
    match parts.next()? {
        "A" => Some(Mark::Prompt),
        "C" => Some(Mark::Output),
        "D" => Some(Mark::End(parts.next().and_then(|s| s.parse().ok()))),
        _ => None,
    }
}

/// One command: absolute lines of its prompt, first output line and end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Block {
    pub(crate) prompt: usize,
    pub(crate) output: Option<usize>,
    pub(crate) end: Option<usize>,
    pub(crate) status: Option<i32>,
}

/// Blocks of one tab, oldest first.
#[derive(Debug, Default)]
pub(crate) struct Blocks {
    list: Vec<Block>,
}

/// Cap on remembered blocks per tab.
const MAX_BLOCKS: usize = 2000;

impl Blocks {
    pub(crate) fn apply(&mut self, mark: Mark, line: usize) {
        match mark {
            Mark::Prompt => {
                if let Some(last) = self.list.last_mut()
                    && last.output.is_some()
                    && last.end.is_none()
                {
                    last.end = Some(line);
                }
                // A prompt without a command (empty line) is replaced.
                if self.list.last().is_some_and(|b| b.output.is_none()) {
                    self.list.pop();
                }
                self.list.push(Block { prompt: line, output: None, end: None, status: None });
                if self.list.len() > MAX_BLOCKS {
                    self.list.remove(0);
                }
            }
            Mark::Output => {
                if let Some(last) = self.list.last_mut() {
                    last.output = Some(line);
                }
            }
            Mark::End(status) => {
                if let Some(last) = self.list.last_mut()
                    && last.output.is_some()
                {
                    last.end = Some(line);
                    last.status = status;
                }
            }
        }
    }

    /// Blocks that ran a command.
    pub(crate) fn commands(&self) -> impl DoubleEndedIterator<Item = &Block> {
        self.list.iter().filter(|b| b.output.is_some())
    }

    /// Prompt line of the closest command above `line`.
    pub(crate) fn prompt_before(&self, line: usize) -> Option<usize> {
        self.commands().rev().map(|b| b.prompt).find(|&p| p < line)
    }

    /// Prompt line of the closest command below `line`.
    pub(crate) fn prompt_after(&self, line: usize) -> Option<usize> {
        self.commands().map(|b| b.prompt).find(|&p| p > line)
    }

    /// Output line range (`start..end`) of the last finished command.
    pub(crate) fn last_output(&self) -> Option<std::ops::Range<usize>> {
        self.commands().rev().find_map(|b| Some(b.output?..b.end?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes(s: &str) -> Segment {
        Segment::Bytes(s.as_bytes().to_vec())
    }

    #[test]
    fn splits_marks_out_of_text() {
        let mut scanner = MarkScanner::default();
        let out = scanner.feed(b"ab\x1b]133;A\x07cd\x1b]133;D;1\x1b\\e");
        assert_eq!(
            out,
            vec![
                bytes("ab"),
                Segment::Mark(Mark::Prompt),
                bytes("cd"),
                Segment::Mark(Mark::End(Some(1))),
                bytes("e"),
            ]
        );
    }

    #[test]
    fn mark_split_across_reads() {
        let mut scanner = MarkScanner::default();
        assert_eq!(scanner.feed(b"x\x1b]13"), vec![bytes("x")]);
        assert_eq!(scanner.feed(b"3;C"), vec![]);
        assert_eq!(scanner.feed(b"\x07y"), vec![Segment::Mark(Mark::Output), bytes("y")]);
    }

    #[test]
    fn other_escapes_pass_through() {
        let mut scanner = MarkScanner::default();
        let input = b"\x1b[31mred\x1b]0;title\x07";
        assert_eq!(scanner.feed(input), vec![Segment::Bytes(input.to_vec())]);
    }

    #[test]
    fn blocks_track_commands() {
        let mut blocks = Blocks::default();
        blocks.apply(Mark::Prompt, 0);
        blocks.apply(Mark::Prompt, 1); // empty line: replaced
        blocks.apply(Mark::Output, 2);
        blocks.apply(Mark::End(Some(0)), 5);
        blocks.apply(Mark::Prompt, 5);
        blocks.apply(Mark::Output, 6);
        blocks.apply(Mark::End(Some(2)), 9);
        blocks.apply(Mark::Prompt, 9);
        let cmds: Vec<_> = blocks.commands().copied().collect();
        assert_eq!(cmds.len(), 2);
        assert_eq!(cmds[0], Block { prompt: 1, output: Some(2), end: Some(5), status: Some(0) });
        assert_eq!(blocks.last_output(), Some(6..9));
        assert_eq!(blocks.prompt_before(5), Some(1));
        assert_eq!(blocks.prompt_after(1), Some(5));
        assert_eq!(blocks.prompt_after(5), None);
    }
}
