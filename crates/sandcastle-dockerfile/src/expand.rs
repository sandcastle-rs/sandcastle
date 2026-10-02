//! Variable expansion and quote removal for instruction arguments, ported
//! from BuildKit's `frontend/dockerfile/shell/lex.go` (default mode).
//!
//! Supported: `$VAR`, `${VAR}`, `${VAR-w}`, `${VAR:-w}`, `${VAR+w}`,
//! `${VAR:+w}`, `${VAR?msg}`, `${VAR:?msg}`, single and double quotes, and
//! the escape character. Pattern operators (`#`, `%`, `/`) are rejected.
//!
//! Intentional deviation from BuildKit: the word inside `${VAR<op>word}` is
//! processed by a nested pass with its own word buffer, so it never splits
//! the enclosing word list; its value is spliced in as one piece.

use std::collections::HashMap;
use std::fmt;
use std::hash::BuildHasher;
use std::iter::Peekable;
use std::str::Chars;

/// Variables visible to [`expand`].
pub trait Env {
    fn get(&self, name: &str) -> Option<&str>;
}

impl<S: BuildHasher> Env for HashMap<String, String, S> {
    fn get(&self, name: &str) -> Option<&str> {
        HashMap::get(self, name).map(String::as_str)
    }
}

/// Deepest `${VAR:-${...}}` nesting accepted; bounds recursion on hostile input.
const MAX_NESTING: usize = 64;

/// Why an argument could not be expanded.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("failed to process {word:?}: {kind}")]
pub struct ExpandError {
    word: String,
    kind: ExpandErrorKind,
}

impl ExpandError {
    pub fn word(&self) -> &str {
        &self.word
    }

    pub fn kind(&self) -> &ExpandErrorKind {
        &self.kind
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ExpandErrorKind {
    UnterminatedSingleQuote,
    UnterminatedDoubleQuote,
    Unterminated(char),
    MissingBrace,
    BadSubstitution,
    UnsupportedModifier(String),
    /// `${...}` nested deeper than the supported limit.
    TooDeep,
    /// `${VAR?msg}` / `${VAR:?msg}` on an unset (or empty) variable.
    Required {
        name: String,
        message: String,
    },
}

impl fmt::Display for ExpandErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnterminatedSingleQuote => {
                f.write_str("unexpected end of statement while looking for matching single-quote")
            }
            Self::UnterminatedDoubleQuote => {
                f.write_str("unexpected end of statement while looking for matching double-quote")
            }
            Self::Unterminated(c) => write!(
                f,
                "unexpected end of statement while looking for matching {c}"
            ),
            Self::MissingBrace => f.write_str("syntax error: missing '}'"),
            Self::BadSubstitution => f.write_str("syntax error: bad substitution"),
            Self::UnsupportedModifier(m) => write!(f, "unsupported modifier ({m}) in substitution"),
            Self::TooDeep => write!(f, "substitutions nested deeper than {MAX_NESTING} levels"),
            Self::Required { name, message } => write!(f, "{name}: {message}"),
        }
    }
}

/// Expands variables and removes quotes and escapes, keeping the result as
/// one string (BuildKit `ProcessWord`). Used for ENV/LABEL values, WORKDIR,
/// USER, EXPOSE.
pub fn expand(word: &str, env: &impl Env, escape: char) -> Result<String, ExpandError> {
    Lexer::new(word, env, escape)
        .run()
        .map(|(result, _)| result)
}

/// Like [`expand`], then splits on unquoted whitespace (BuildKit
/// `ProcessWords`). Used for COPY sources and destination.
pub fn expand_words(word: &str, env: &impl Env, escape: char) -> Result<Vec<String>, ExpandError> {
    Lexer::new(word, env, escape).run().map(|(_, words)| words)
}

struct Lexer<'a, E> {
    source: &'a str,
    chars: Peekable<Chars<'a>>,
    env: &'a E,
    escape: char,
    depth: usize,
}

/// Word splitting that follows BuildKit's `wordsStruct`: characters from
/// variables split on whitespace, quoted or escaped ones never do.
#[derive(Default)]
struct Words {
    words: Vec<String>,
    buf: String,
    in_word: bool,
}

impl Words {
    fn add_char(&mut self, c: char) {
        if c.is_whitespace() {
            if self.in_word && !self.buf.is_empty() {
                self.words.push(std::mem::take(&mut self.buf));
                self.in_word = false;
            }
        } else {
            self.add_raw_char(c);
        }
    }

    fn add_raw_char(&mut self, c: char) {
        self.buf.push(c);
        self.in_word = true;
    }

    fn add_str(&mut self, s: &str) {
        s.chars().for_each(|c| self.add_char(c));
    }

    fn add_raw_str(&mut self, s: &str) {
        self.buf.push_str(s);
        self.in_word = true;
    }

    fn finish(mut self) -> Vec<String> {
        if !self.buf.is_empty() {
            self.words.push(self.buf);
        }
        self.words
    }
}

impl<'a, E: Env> Lexer<'a, E> {
    fn new(source: &'a str, env: &'a E, escape: char) -> Self {
        Self {
            source,
            chars: source.chars().peekable(),
            env,
            escape,
            depth: 0,
        }
    }

    fn run(mut self) -> Result<(String, Vec<String>), ExpandError> {
        self.until(None).map_err(|kind| ExpandError {
            word: self.source.to_owned(),
            kind,
        })
    }

    fn peek(&mut self) -> Option<char> {
        self.chars.peek().copied()
    }

    /// Processes input until `stop` (consumed) or the end of input.
    fn until(&mut self, stop: Option<char>) -> Result<(String, Vec<String>), ExpandErrorKind> {
        let mut result = String::new();
        let mut words = Words::default();
        while let Some(ch) = self.peek() {
            if Some(ch) == stop {
                self.chars.next();
                return Ok((result, words.finish()));
            }
            match ch {
                '$' => {
                    let value = self.dollar()?;
                    words.add_str(&value);
                    result.push_str(&value);
                }
                '<' => {
                    let value = self.possible_heredoc();
                    words.add_raw_str(&value);
                    result.push_str(&value);
                }
                '\'' => {
                    let value = self.single_quote()?;
                    words.add_raw_str(&value);
                    result.push_str(&value);
                }
                '"' => {
                    let value = self.double_quote()?;
                    words.add_raw_str(&value);
                    result.push_str(&value);
                }
                _ => {
                    self.chars.next();
                    if ch == self.escape {
                        // An escape at the very end is dropped.
                        let Some(next) = self.chars.next() else { break };
                        words.add_raw_char(next);
                        result.push(next);
                    } else {
                        words.add_char(ch);
                        result.push(ch);
                    }
                }
            }
        }
        match stop {
            Some(c) => Err(ExpandErrorKind::Unterminated(c)),
            None => Ok((result, words.finish())),
        }
    }

    /// Everything up to the next `'` is literal; the escape is not special.
    fn single_quote(&mut self) -> Result<String, ExpandErrorKind> {
        self.chars.next();
        let mut result = String::new();
        loop {
            match self.chars.next() {
                None => return Err(ExpandErrorKind::UnterminatedSingleQuote),
                Some('\'') => return Ok(result),
                Some(c) => result.push(c),
            }
        }
    }

    /// `$` expands; the escape only escapes `"`, `$` and itself.
    fn double_quote(&mut self) -> Result<String, ExpandErrorKind> {
        self.chars.next();
        let mut result = String::new();
        loop {
            match self.peek() {
                None => return Err(ExpandErrorKind::UnterminatedDoubleQuote),
                Some('"') => {
                    self.chars.next();
                    return Ok(result);
                }
                Some('$') => result.push_str(&self.dollar()?),
                Some(_) => {
                    let mut ch = self.chars.next().expect("peeked");
                    if ch == self.escape {
                        match self.peek() {
                            None => continue,
                            Some(next) if next == '"' || next == '$' || next == self.escape => {
                                ch = self.chars.next().expect("peeked");
                            }
                            Some(_) => {}
                        }
                    }
                    result.push(ch);
                }
            }
        }
    }

    fn dollar(&mut self) -> Result<String, ExpandErrorKind> {
        self.chars.next();
        if self.peek() != Some('{') {
            let name = self.name();
            if name.is_empty() {
                return Ok("$".to_owned());
            }
            return Ok(self.lookup(&name).unwrap_or_default().to_owned());
        }
        self.chars.next();
        match self.peek() {
            None => return Err(ExpandErrorKind::MissingBrace),
            Some('{' | '}' | ':') => return Err(ExpandErrorKind::BadSubstitution),
            Some(_) => {}
        }
        let name = self.name();
        let (null_is_unset, op) = match self.chars.next() {
            None => return Err(ExpandErrorKind::MissingBrace),
            Some('}') => return Ok(self.lookup(&name).unwrap_or_default().to_owned()),
            Some(':') => (true, self.chars.next()),
            Some(op) => (false, Some(op)),
        };
        let op = match op {
            Some(op @ ('-' | '+' | '?')) => op,
            Some(other) => {
                let prefix = if null_is_unset { ":" } else { "" };
                return Err(ExpandErrorKind::UnsupportedModifier(format!(
                    "{prefix}{other}"
                )));
            }
            None => return Err(ExpandErrorKind::MissingBrace),
        };
        if self.depth >= MAX_NESTING {
            return Err(ExpandErrorKind::TooDeep);
        }
        self.depth += 1;
        let word = self.until(Some('}'));
        self.depth -= 1;
        let (word, _) = word.map_err(|e| match e {
            ExpandErrorKind::Unterminated('}') => ExpandErrorKind::MissingBrace,
            e => e,
        })?;
        let value = self.lookup(&name);
        let missing = match value {
            None => true,
            Some(v) => null_is_unset && v.is_empty(),
        };
        match op {
            '-' => Ok(if missing {
                word
            } else {
                value.unwrap_or_default().to_owned()
            }),
            '+' => Ok(if missing { String::new() } else { word }),
            _ if !missing => Ok(value.unwrap_or_default().to_owned()),
            _ => {
                let default = if value.is_none() {
                    "is not allowed to be unset"
                } else {
                    "is not allowed to be empty"
                };
                let message = if word.is_empty() {
                    default.to_owned()
                } else {
                    word
                };
                Err(ExpandErrorKind::Required { name, message })
            }
        }
    }

    /// A variable name: letters, digits and `_`; a run of digits
    /// (positional parameter); or one special parameter character.
    fn name(&mut self) -> String {
        let mut name = String::new();
        while let Some(c) = self.peek() {
            if name.is_empty() && c.is_numeric() {
                while let Some(d) = self.chars.next_if(|c| c.is_numeric()) {
                    name.push(d);
                }
                return name;
            }
            if name.is_empty() && matches!(c, '@' | '*' | '#' | '?' | '-' | '$' | '!' | '0') {
                self.chars.next();
                return c.to_string();
            }
            if !(c.is_alphabetic() || c.is_numeric() || c == '_') {
                break;
            }
            self.chars.next();
            name.push(c);
        }
        name
    }

    /// `<<` plus following blanks stays one unsplittable piece.
    fn possible_heredoc(&mut self) -> String {
        self.chars.next();
        if self.peek() != Some('<') {
            return "<".to_owned();
        }
        self.chars.next();
        let mut result = "<<".to_owned();
        while let Some(c) = self.chars.next_if(|c| matches!(c, '\t' | '\r' | ' ')) {
            result.push(c);
        }
        result
    }

    fn lookup(&self, name: &str) -> Option<&'a str> {
        self.env.get(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env() -> HashMap<String, String> {
        [("A", "1"), ("EMPTY", "")]
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect()
    }

    #[test]
    fn deep_nesting_is_an_error_not_an_abort() {
        let err = expand(&"${A:-".repeat(100_000), &env(), '\\').unwrap_err();
        assert_eq!(err.kind(), &ExpandErrorKind::TooDeep);
        let ten = "${X:-".repeat(10) + "ok" + &"}".repeat(10);
        assert_eq!(expand(&ten, &env(), '\\').unwrap(), "ok");
        let at_cap = "${X:-".repeat(MAX_NESTING) + "ok" + &"}".repeat(MAX_NESTING);
        assert_eq!(expand(&at_cap, &env(), '\\').unwrap(), "ok");
        let over = "${X:-".repeat(MAX_NESTING + 1) + "ok" + &"}".repeat(MAX_NESTING + 1);
        assert_eq!(
            expand(&over, &env(), '\\').unwrap_err().kind(),
            &ExpandErrorKind::TooDeep
        );
    }

    #[test]
    fn backtick_escape_is_honoured() {
        assert_eq!(expand("a`$A b\\c", &env(), '`').unwrap(), "a$A b\\c");
        assert_eq!(expand("\"x`\"y\"", &env(), '`').unwrap(), "x\"y");
    }

    #[test]
    fn pattern_operators_are_rejected() {
        for word in ["${A#x}", "${A%x}", "${A/x/y}", "${A:#x}"] {
            assert!(expand(word, &env(), '\\').is_err(), "{word}");
        }
    }

    #[test]
    fn required_variables() {
        let err = expand("${MISSING?need it}", &env(), '\\').unwrap_err();
        assert_eq!(
            err.to_string(),
            "failed to process \"${MISSING?need it}\": MISSING: need it"
        );
        assert_eq!(expand("${EMPTY?}", &env(), '\\').unwrap(), "");
        assert!(expand("${EMPTY:?}", &env(), '\\').is_err());
    }

    #[test]
    fn variables_split_but_quotes_do_not() {
        let env: HashMap<String, String> = [("V".to_owned(), "a  b".to_owned())].into();
        assert_eq!(
            expand_words("x$V \"$V\" 'c d'", &env, '\\').unwrap(),
            ["xa", "b", "a  b", "c d"]
        );
        assert_eq!(expand_words("p${X:-q}", &env, '\\').unwrap(), ["pq"]);
    }
}
