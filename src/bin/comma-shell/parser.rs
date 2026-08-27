//! Parser: tokens -> AST (Script -> AndOr -> Pipeline -> Command).

pub use crate::lexer::ParseError;
use crate::lexer::{self, Part, Token};

/// A whole command line: `AndOr` chains separated by `;`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Script {
    pub seq: Vec<AndOr>,
}

/// Pipelines chained by `&&` / `||`; each next one runs conditionally on the
/// previous exit status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AndOr {
    pub first: Pipeline,
    pub rest: Vec<(Connector, Pipeline)>,
}

/// `&&` (run next on success) or `||` (run next on failure).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Connector {
    And,
    Or,
}

/// Commands connected by `|`. Invariant: `cmds` is never empty.
/// `background` (`cmd &`) runs the pipeline detached from the terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pipeline {
    pub cmds: Vec<Command>,
    pub background: bool,
}

/// One command: unexpanded argv words plus redirects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    pub argv: Vec<Vec<Part>>,
    pub redirects: Vec<Redirect>,
}

/// A redirect; the target word is expanded at execution time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Redirect {
    In(Vec<Part>),
    Out { target: Vec<Part>, append: bool },
    ErrOut { target: Vec<Part>, append: bool },
}

pub fn parse(input: &str) -> Result<Script, ParseError> {
    let tokens = lexer::lex(input)?;
    let mut parser = Parser { tokens, pos: 0 };
    parser.script()
}

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }

    fn next(&mut self) -> Option<Token> {
        let token = self.tokens.get(self.pos).cloned();
        if token.is_some() {
            self.pos += 1;
        }
        token
    }

    fn script(&mut self) -> Result<Script, ParseError> {
        let mut seq = Vec::new();
        loop {
            match self.peek() {
                None => break,
                Some(Token::Semi) => {
                    // Allow stray/trailing semicolons.
                    self.pos += 1;
                }
                Some(Token::Amp) => return Err(ParseError::UnexpectedAmp),
                _ => {
                    let mut and_or = self.and_or()?;
                    if matches!(self.peek(), Some(Token::Amp)) {
                        // Trailing `&` backgrounds the whole command chain.
                        self.pos += 1;
                        and_or.first.background = true;
                        for (_, pipeline) in &mut and_or.rest {
                            pipeline.background = true;
                        }
                        // Only `;` or the end of the line may follow `&`;
                        // `a & b` is rejected to keep jobs one-per-command.
                        match self.peek() {
                            None | Some(Token::Semi) => {}
                            _ => return Err(ParseError::UnexpectedAmp),
                        }
                    }
                    seq.push(and_or);
                }
            }
        }
        Ok(Script { seq })
    }

    fn and_or(&mut self) -> Result<AndOr, ParseError> {
        let first = self.pipeline()?;
        let mut rest = Vec::new();
        loop {
            let connector = match self.peek() {
                Some(Token::And) => Connector::And,
                Some(Token::Or) => Connector::Or,
                _ => break,
            };
            self.pos += 1;
            rest.push((connector, self.pipeline()?));
        }
        Ok(AndOr { first, rest })
    }

    fn pipeline(&mut self) -> Result<Pipeline, ParseError> {
        let mut cmds = vec![self.command()?];
        while matches!(self.peek(), Some(Token::Pipe)) {
            self.pos += 1;
            cmds.push(self.command()?);
        }
        Ok(Pipeline { cmds, background: false })
    }

    fn command(&mut self) -> Result<Command, ParseError> {
        let mut argv = Vec::new();
        let mut redirects = Vec::new();
        loop {
            match self.next() {
                Some(Token::Word(parts)) => argv.push(parts),
                Some(Token::Out { append }) => {
                    redirects.push(Redirect::Out { target: self.redirect_target()?, append });
                }
                Some(Token::ErrOut { append }) => {
                    redirects.push(Redirect::ErrOut { target: self.redirect_target()?, append });
                }
                Some(Token::In) => {
                    redirects.push(Redirect::In(self.redirect_target()?));
                }
                Some(_) => {
                    // Operator ends the command; put it back for the caller.
                    self.pos -= 1;
                    break;
                }
                None => break,
            }
        }
        if argv.is_empty() && redirects.is_empty() {
            return Err(ParseError::ExpectedCommand);
        }
        Ok(Command { argv, redirects })
    }

    fn redirect_target(&mut self) -> Result<Vec<Part>, ParseError> {
        match self.next() {
            Some(Token::Word(parts)) => Ok(parts),
            _ => Err(ParseError::MissingRedirectTarget),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simple_command() {
        let script = parse("echo hello").unwrap();
        assert_eq!(script.seq.len(), 1);
        let cmd = &script.seq[0].first.cmds[0];
        assert_eq!(
            cmd.argv,
            vec![vec![Part::Lit("echo".into())], vec![Part::Lit("hello".into())]]
        );
        assert!(cmd.redirects.is_empty());
    }

    #[test]
    fn pipeline_and_connectors() {
        let script = parse("a | b | c && d ; e || f").unwrap();
        assert_eq!(script.seq.len(), 2);
        assert_eq!(script.seq[0].first.cmds.len(), 3);
        assert_eq!(script.seq[0].rest.len(), 1);
        assert_eq!(script.seq[0].rest[0].0, Connector::And);
        assert_eq!(script.seq[1].first.cmds.len(), 1);
        assert_eq!(script.seq[1].rest.len(), 1);
        assert_eq!(script.seq[1].rest[0].0, Connector::Or);
    }

    #[test]
    fn redirects() {
        let script = parse("cmd < in > out 2>> err").unwrap();
        let cmd = &script.seq[0].first.cmds[0];
        assert_eq!(
            cmd.redirects,
            vec![
                Redirect::In(vec![Part::Lit("in".into())]),
                Redirect::Out { target: vec![Part::Lit("out".into())], append: false },
                Redirect::ErrOut { target: vec![Part::Lit("err".into())], append: true },
            ]
        );
    }

    #[test]
    fn errors() {
        assert!(parse("| a").is_err());
        assert!(parse("a |").is_err());
        assert!(parse("a > ").is_err());
        assert!(parse("a && && b").is_err());
    }

    #[test]
    fn trailing_amp_backgrounds_the_pipeline() {
        let script = parse("sleep 100 &").unwrap();
        assert!(script.seq[0].first.background);

        let script = parse("a | b & ; c").unwrap();
        assert!(script.seq[0].first.background);
        assert!(!script.seq[1].first.background);

        // `&` backgrounds the whole `&&`/`||` chain.
        let script = parse("a && b &").unwrap();
        assert!(script.seq[0].first.background);
        assert!(script.seq[0].rest[0].1.background);
    }

    #[test]
    fn amp_in_the_middle_is_an_error() {
        assert_eq!(parse("a & b"), Err(ParseError::UnexpectedAmp));
        assert_eq!(parse("a & | b"), Err(ParseError::UnexpectedAmp));
        assert_eq!(parse("& a"), Err(ParseError::UnexpectedAmp));
    }

    #[test]
    fn empty_input() {
        assert_eq!(parse("").unwrap().seq.len(), 0);
        assert_eq!(parse("  ; ").unwrap().seq.len(), 0);
    }
}
