//! Line handling and the parse loop, ported from BuildKit's
//! `frontend/dockerfile/parser/parser.go` and `directives.go`.

use std::fmt;

use crate::line_parsers::{node_from_line, parse_words};

/// Default escape character; `# escape=` can switch it to a backtick.
const DEFAULT_ESCAPE: char = '\\';
const BOM: char = '\u{feff}';

/// A parsed Dockerfile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dockerfile {
    /// Escape character in effect (`\` or `` ` ``), needed by [`crate::expand`].
    pub escape: char,
    /// One node per instruction, in file order.
    pub nodes: Vec<Node>,
}

/// One instruction as written, before typing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    /// Instruction name with its original case, e.g. `RUN` or `run`.
    pub cmd: String,
    /// Builder flags such as `--platform=linux/amd64`, verbatim.
    pub flags: Vec<String>,
    /// Arguments after the per-instruction split. Quotes and escapes are
    /// kept; [`crate::expand`] removes them.
    pub args: Vec<String>,
    /// Trigger instruction of `ONBUILD`.
    pub sub: Option<Box<Node>>,
    /// The arguments were a JSON array (exec form).
    pub json: bool,
    /// The logical line the node was parsed from, continuations joined.
    pub original: String,
    /// First and last physical line (1-based, inclusive).
    pub start_line: usize,
    pub end_line: usize,
}

/// Why a Dockerfile could not be parsed, and on which line.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("line {line}: {kind}")]
pub struct ParseError {
    line: usize,
    kind: ParseErrorKind,
}

impl ParseError {
    pub(crate) fn new(line: usize, kind: ParseErrorKind) -> Self {
        Self { line, kind }
    }

    /// 1-based line the error was found on.
    pub fn line(&self) -> usize {
        self.line
    }

    pub fn kind(&self) -> &ParseErrorKind {
        &self.kind
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ParseErrorKind {
    NoInstructions,
    InvalidEscape(String),
    DuplicateDirective(String),
    /// Legacy `ENV key value` / `LABEL key value` with only a key.
    MissingValue(&'static str),
    /// `key=value` form with a word lacking `=`.
    MissingEquals(String),
    /// A JSON array with a non-string element.
    NotStringArray,
    /// BuildKit would read a heredoc body here; heredocs are not supported yet.
    Heredoc,
}

impl fmt::Display for ParseErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoInstructions => f.write_str("file with no instructions"),
            Self::InvalidEscape(v) => {
                write!(f, "invalid escape token '{v}' does not match ` or \\")
            }
            Self::DuplicateDirective(k) => write!(f, "only one {k} parser directive can be used"),
            Self::MissingValue(cmd) => write!(f, "{cmd} must have two arguments"),
            Self::MissingEquals(w) => {
                write!(
                    f,
                    "Syntax error - can't find = in {w:?}. Must be of the form: name=value"
                )
            }
            Self::NotStringArray => f.write_str(
                "when using JSON array syntax, arrays must be comprised of strings only",
            ),
            Self::Heredoc => f.write_str("heredocs are not supported yet"),
        }
    }
}

/// Parses Dockerfile text into nodes.
pub fn parse(src: &str) -> Result<Dockerfile, ParseError> {
    let mut directives = Directives::default();
    let mut lines = src.split_inclusive('\n');
    let mut current = 0;
    let mut nodes = Vec::new();

    while let Some(raw) = lines.next() {
        let raw = if current == 0 {
            raw.strip_prefix(BOM).unwrap_or(raw)
        } else {
            raw
        };
        let line = process_line(&mut directives, raw, true)
            .map_err(|k| ParseError::new(current + 1, k))?;
        current += 1;
        let start = current;

        let (first, mut end_of_line) = trim_continuation(line, directives.escape);
        if end_of_line && first.is_empty() {
            continue;
        }
        let mut logical = first.to_owned();
        while !end_of_line {
            let Some(raw) = lines.next() else { break };
            let line = process_line(&mut directives, raw, false)
                .map_err(|k| ParseError::new(current + 1, k))?;
            current += 1;
            // Comments and blank lines inside a continuation are dropped.
            if is_comment(raw) || is_blank(line) {
                continue;
            }
            let (part, eol) = trim_continuation(line, directives.escape);
            logical.push_str(part);
            end_of_line = eol;
        }

        let mut node =
            node_from_line(&logical, directives.escape).map_err(|k| ParseError::new(start, k))?;
        if may_read_heredoc(&node, &logical, directives.escape) {
            return Err(ParseError::new(start, ParseErrorKind::Heredoc));
        }
        node.start_line = start;
        node.end_line = current;
        nodes.push(node);
    }

    if nodes.is_empty() {
        return Err(ParseError::new(current, ParseErrorKind::NoInstructions));
    }
    Ok(Dockerfile {
        escape: directives.escape,
        nodes,
    })
}

/// Strips the line ending (and, for the first line of an instruction,
/// leading whitespace), feeds the parser-directive scanner, and blanks
/// comment lines.
fn process_line<'a>(
    d: &mut Directives,
    raw: &'a str,
    strip_left: bool,
) -> Result<&'a str, ParseErrorKind> {
    let mut token = raw.trim_end_matches(['\r', '\n']);
    if strip_left {
        token = token.trim_start();
    }
    d.scan(token)?;
    Ok(if is_comment(token) { "" } else { token })
}

fn is_comment(line: &str) -> bool {
    line.trim_start().starts_with('#')
}

fn is_blank(line: &str) -> bool {
    line.trim_end_matches(['\r', '\n']).trim_start().is_empty()
}

/// Removes a trailing escape character (plus trailing blanks after it) and
/// reports whether the logical line ends here. Equivalent to BuildKit's
/// `([^E])E[ \t]*$|^E[ \t]*$` with `E` the escape character: an escape
/// preceded by another escape is literal.
fn trim_continuation(line: &str, escape: char) -> (&str, bool) {
    let Some(before) = line.trim_end_matches([' ', '\t']).strip_suffix(escape) else {
        return (line, true);
    };
    match before.chars().next_back() {
        None => ("", false),
        Some(c) if c == escape => (line, true),
        Some(_) => (before, false),
    }
}

/// True when BuildKit would treat a word of this line as a heredoc start
/// (`<<EOF`, `<<-EOF`, `2<<EOF`, `<< EOF`) for ADD, COPY or shell-form RUN,
/// directly or under ONBUILD.
fn may_read_heredoc(node: &Node, line: &str, escape: char) -> bool {
    let target = match (&node.sub, node.cmd.eq_ignore_ascii_case("onbuild")) {
        (Some(sub), true) => sub.as_ref(),
        _ => node,
    };
    let kind = target.cmd.to_ascii_lowercase();
    if !matches!(kind.as_str(), "add" | "copy" | "run") || target.json || !line.contains("<<") {
        return false;
    }
    let words = parse_words(line, escape);
    words.iter().enumerate().any(|(i, word)| {
        let rest = word.trim_start_matches(|c: char| c.is_ascii_digit());
        let Some(rest) = rest.strip_prefix("<<") else {
            return false;
        };
        let rest = rest.strip_prefix('-').unwrap_or(rest);
        let name = if rest.is_empty() {
            words.get(i + 1).map_or("", String::as_str)
        } else {
            rest
        };
        !name.is_empty() && !name.contains('<')
    })
}

/// Parser directives (`# escape=`, `# syntax=`, `# check=`): only
/// recognised in the leading comment block, each at most once.
struct Directives {
    escape: char,
    done: bool,
    seen: Vec<String>,
}

impl Default for Directives {
    fn default() -> Self {
        Self {
            escape: DEFAULT_ESCAPE,
            done: false,
            seen: Vec::new(),
        }
    }
}

impl Directives {
    fn scan(&mut self, line: &str) -> Result<(), ParseErrorKind> {
        if self.done {
            return Ok(());
        }
        let Some((key, value)) = line
            .strip_prefix('#')
            .and_then(|rest| split_directive(rest.trim_start()))
        else {
            self.done = true;
            return Ok(());
        };
        let key = key.to_ascii_lowercase();
        if !matches!(key.as_str(), "syntax" | "check" | "escape") {
            self.done = true;
            return Ok(());
        }
        if self.seen.contains(&key) {
            return Err(ParseErrorKind::DuplicateDirective(key));
        }
        if key == "escape" {
            self.escape = match value {
                "\\" => '\\',
                "`" => '`',
                _ => return Err(ParseErrorKind::InvalidEscape(value.to_owned())),
            };
        }
        self.seen.push(key);
        Ok(())
    }
}

/// Go RE2 `\s`: ASCII space, tab, newline, form feed, carriage return.
fn is_re2_space(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\x0c' | '\r')
}

/// Matches BuildKit's `^([a-zA-Z][a-zA-Z0-9]*)\s*=\s*(.+?)\s*$`.
fn split_directive(s: &str) -> Option<(&str, &str)> {
    if !s.starts_with(|c: char| c.is_ascii_alphabetic()) {
        return None;
    }
    let key_len = s
        .find(|c: char| !c.is_ascii_alphanumeric())
        .unwrap_or(s.len());
    let (key, rest) = s.split_at(key_len);
    let after_eq = rest.trim_start_matches(is_re2_space).strip_prefix('=')?;
    let value = after_eq.trim_matches(is_re2_space);
    if !value.is_empty() {
        return Some((key, value));
    }
    // Only whitespace after `=`: backtracking gives `.+?` the last character.
    let last = after_eq.chars().next_back()?;
    Some((key, &after_eq[after_eq.len() - last.len_utf8()..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn continuation_rules_follow_buildkit_regex() {
        assert_eq!(trim_continuation("echo a \\", '\\'), ("echo a ", false));
        assert_eq!(trim_continuation("echo a \\ \t", '\\'), ("echo a ", false));
        assert_eq!(trim_continuation("\\", '\\'), ("", false));
        assert_eq!(trim_continuation("a\\\\", '\\'), ("a\\\\", true));
        assert_eq!(trim_continuation("a `", '`'), ("a ", false));
        assert_eq!(trim_continuation("a \\", '`'), ("a \\", true));
    }

    #[test]
    fn directive_regex_edge_cases() {
        assert_eq!(split_directive("escape = `"), Some(("escape", "`")));
        assert_eq!(
            split_directive("syntax=docker/dockerfile:1  "),
            Some(("syntax", "docker/dockerfile:1"))
        );
        assert_eq!(split_directive("escape=   "), Some(("escape", " ")));
        assert_eq!(split_directive("escape="), None);
        assert_eq!(split_directive("1escape=x"), None);
        assert_eq!(split_directive("There is no directive"), None);
    }

    #[test]
    fn invalid_escape_directive_is_rejected() {
        let err = parse("# escape=x\nFROM a\n").unwrap_err();
        assert_eq!(err.line(), 1);
        assert_eq!(err.kind(), &ParseErrorKind::InvalidEscape("x".into()));
    }

    #[test]
    fn duplicate_directive_is_rejected() {
        let err = parse("# escape=`\n# escape=\\\nFROM a\n").unwrap_err();
        assert_eq!(err.line(), 2);
        assert_eq!(
            err.kind(),
            &ParseErrorKind::DuplicateDirective("escape".into())
        );
    }

    #[test]
    fn directive_after_instruction_is_a_comment() {
        let df = parse("FROM a\n# escape=`\nRUN b \\\n c\n").unwrap();
        assert_eq!(df.escape, '\\');
        assert_eq!(df.nodes[1].args, ["b  c"]);
    }

    #[test]
    fn bom_and_crlf_are_ignored() {
        let df = parse("\u{feff}FROM a\r\nRUN b\r\n").unwrap();
        assert_eq!(df.nodes[0].args, ["a"]);
        assert_eq!(df.nodes[1].args, ["b"]);
    }

    #[test]
    fn heredocs_are_rejected_not_misparsed() {
        for src in [
            "RUN <<EOF\necho hi\nEOF\n",
            "RUN cat <<-EOT\nx\nEOT\n",
            "COPY <<EOF /x\nhi\nEOF\n",
            "RUN 3<<EOF\nx\nEOF\n",
            "RUN << EOF\nx\nEOF\n",
            "ONBUILD RUN <<EOF\nx\nEOF\n",
        ] {
            let err = parse(&format!("FROM a\n{src}")).unwrap_err();
            assert_eq!(err.kind(), &ParseErrorKind::Heredoc, "{src:?}");
            assert_eq!(err.line(), 2, "{src:?}");
        }
        for src in [
            "RUN echo \"<<EOF\"\n",
            "RUN echo <<<x\n",
            "RUN [\"cat\", \"<<EOF\"]\n",
            "CMD cat <<EOF\n",
        ] {
            parse(&format!("FROM a\n{src}")).unwrap_or_else(|e| panic!("{src:?}: {e}"));
        }
    }

    #[test]
    fn line_ranges_cover_continuations() {
        let df = parse("FROM a\n\nRUN b \\\n  # note\n\n  c\nCMD d\n").unwrap();
        let ranges: Vec<_> = df
            .nodes
            .iter()
            .map(|n| (n.start_line, n.end_line))
            .collect();
        assert_eq!(ranges, [(1, 1), (3, 6), (7, 7)]);
    }
}
