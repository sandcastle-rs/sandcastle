# Dockerfile Parser Crate Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A pure library crate `sandcastle-dockerfile` that parses Dockerfiles exactly like BuildKit, types the v1 instructions, and expands variables — verified against BuildKit's own fixtures.

**Architecture:** A port of BuildKit's `frontend/dockerfile/parser` (line handling, parser directives, per-instruction argument splitting → `Node`), a typed layer following `frontend/dockerfile/instructions/parse.go` (`Instruction::try_from(&Node)`), and a port of `frontend/dockerfile/shell/lex.go` (`expand`, `expand_words`). BuildKit's parser and lexer fixtures are vendored at a pinned commit and run as table tests against a test-only dump in BuildKit's `Node.Dump()` format. Nothing is wired into the host binary (that is plan 4).

**Tech Stack:** Rust 2024 (workspace `rust-version` 1.89), serde_json 1.0.151 (JSON exec-form arrays only), thiserror 2.0.21 (public error types). BuildKit commit `82a1f40db9624f03b94bc2a42f0e5bac0e89aad7` (Apache-2.0) for fixtures.

**Spec:** `docs/superpowers/specs/2026-10-02-sandcastle-v1-design.md` (section "Dockerfile parser (`sandcastle-dockerfile`)").

## Global Constraints

- Crate `crates/sandcastle-dockerfile`, workspace member, `edition.workspace = true`, `rust-version.workspace = true`.
- Dependencies: `serde_json = "1.0.151"`, `thiserror = "2.0.21"`. No other dependencies, no dev-dependencies.
- No I/O in library code (`&str` in, values out); only `#[cfg(test)]` code reads `testdata/`. `#![forbid(unsafe_code)]`.
- Public API exactly: `parse(&str) -> Result<Dockerfile, ParseError>`; `Dockerfile { escape: char, nodes: Vec<Node> }`; `Node { cmd, flags, args, sub, json, original, start_line, end_line }`; `Instruction::try_from(&Node) -> Result<Instruction, InstructionError>` with variants `From, Run, Copy, Env, Label, Workdir, User, Expose, Cmd, Entrypoint, Other(Node)`; `expand(&str, &impl Env, escape: char) -> Result<String, ExpandError>`; `expand_words(&str, &impl Env, escape: char) -> Result<Vec<String>, ExpandError>`; trait `Env`.
- Heredocs are out of scope: a line BuildKit would read a heredoc body for must fail with `ParseErrorKind::Heredoc`, never be misparsed.
- Fixtures vendored byte-exact from moby/buildkit `82a1f40db9624f03b94bc2a42f0e5bac0e89aad7`; their `result` files are the oracle (no hand-written expected dumps for fixture cases).
- Intentional deviations from BuildKit (keep them, each has a code comment): builder-flag scanning walks chars, not bytes (non-ASCII flag values stay intact); nested `${...}` expansion uses its own word buffer (BuildKit's shared buffer drops text); pattern operators `#`, `%`, `/` are rejected; no 64 KiB line limit; no warnings or `PrevComment`.
- Do not touch the host crate (`src/`), the guest, or the proto crate. Only the workspace `members` list, `NOTICE`, and `Cargo.lock` change outside the new crate.
- Lint gate per task: `cargo fmt --all --check` and `cargo clippy -p sandcastle-dockerfile --all-targets -- -D warnings`.
- Tests: no tautological tests; never run the README conformance suite.

## Review Focus

1. A Dockerfile using heredocs (`RUN <<EOF`) must fail with a clear "heredocs are not supported yet" error at the right line, not silently turn the body lines into ignored instructions — pinned by `parser::tests::heredocs_are_rejected_not_misparsed` (Task 1).
2. Flag values with non-ASCII text (`COPY --chown=jürgen ...`) must survive intact (BuildKit re-encodes them byte by byte) — pinned by `line_parsers::tests::flags_stop_at_double_dash_and_keep_unicode` (Task 1).
3. Dockerfiles saved on Windows (CRLF line endings, UTF-8 BOM) must parse like LF files — pinned by `parser::tests::bom_and_crlf_are_ignored` (Task 1).
4. A typo'd instruction (`FROOM`) or flag (`COPY --mode=1`) must be an error naming it, with the instruction's line, not be ignored — pinned by `instruction::tests::other_and_unknown_instructions` and `copy_needs_a_destination_and_known_flags` (Task 3).
5. With the `escape` directive set to a backtick, expansion must use the backtick as escape, not the backslash — pinned by `expand::tests::backtick_escape_is_honoured` (Task 4).

---

## File Structure

```
Cargo.toml                                  + workspace member
NOTICE                                      + BuildKit attribution
crates/sandcastle-dockerfile/
  Cargo.toml
  .gitattributes                            testdata is byte-exact (-text)
  src/lib.rs                                module list, re-exports, crate docs
  src/parser.rs                             parse loop, directives, continuation, heredoc guard, Dockerfile/Node/ParseError
  src/line_parsers.rs                       split_command, builder flags, parse_words, per-instruction argument splitting
  src/instruction.rs                        Instruction, Command, Flag, KeyValue, InstructionError, flag validation
  src/expand.rs                             Env, expand, expand_words, ExpandError (shell lexer port)
  src/dump.rs                               #[cfg(test)] BuildKit Node.Dump() format + Go strconv.Quote
  src/buildkit_fixtures.rs                  #[cfg(test)] table tests over testdata/buildkit
  testdata/buildkit/SOURCE                  repo + pinned commit + vendored paths
  testdata/buildkit/LICENSE                 BuildKit Apache-2.0 license
  testdata/buildkit/parser/testfiles/       33 cases: Dockerfile + result
  testdata/buildkit/parser/testfiles-negative/  4 cases that must fail
  testdata/buildkit/parser/testfile-line/Dockerfile
  testdata/buildkit/shell/envVarTest
  testdata/buildkit/shell/wordsTest
```

---

### Task 1: Crate scaffold and parser

**Files:**
- Modify: `Cargo.toml` (workspace `members`)
- Create: `crates/sandcastle-dockerfile/Cargo.toml`, `crates/sandcastle-dockerfile/src/lib.rs`, `crates/sandcastle-dockerfile/src/parser.rs`, `crates/sandcastle-dockerfile/src/line_parsers.rs`

**Interfaces:**
- Produces: `parse(src: &str) -> Result<Dockerfile, ParseError>`; `pub struct Dockerfile { pub escape: char, pub nodes: Vec<Node> }`; `pub struct Node { pub cmd: String, pub flags: Vec<String>, pub args: Vec<String>, pub sub: Option<Box<Node>>, pub json: bool, pub original: String, pub start_line: usize, pub end_line: usize }`; `ParseError::{line(&self) -> usize, kind(&self) -> &ParseErrorKind}`; `ParseErrorKind::{NoInstructions, InvalidEscape(String), DuplicateDirective(String), MissingValue(&'static str), MissingEquals(String), NotStringArray, Heredoc}` (`#[non_exhaustive]`). Node conventions later tasks rely on: ENV/LABEL `args` are `[key, value, sep]` triples (`sep` `""` legacy or `"="`); exec form sets `json = true`; ONBUILD's trigger is in `sub`; unknown instructions get `args == [""]`.

- [ ] **Step 1: Add the crate to the workspace**

In the root `Cargo.toml`, change the members line to:

```toml
members = [".", "crates/sandcastle-proto", "crates/sandcastle-guest", "crates/sandcastle-dockerfile"]
```

`crates/sandcastle-dockerfile/Cargo.toml`:

```toml
[package]
name = "sandcastle-dockerfile"
version = "0.1.0"
edition.workspace = true
rust-version.workspace = true

[dependencies]
serde_json = "1.0.151"
thiserror = "2.0.21"
```

`crates/sandcastle-dockerfile/src/lib.rs`:

```rust
//! Dockerfile parsing for sandcastle.
//!
//! `parse` turns Dockerfile text into `Node`s with the same rules as
//! BuildKit's `frontend/dockerfile/parser`; `Instruction::try_from` gives the
//! typed form of one node, and `expand` / `expand_words` perform BuildKit's
//! variable expansion and quote removal on an argument. The crate does no I/O.

#![forbid(unsafe_code)]

mod line_parsers;
mod parser;

pub use parser::{Dockerfile, Node, ParseError, ParseErrorKind, parse};
```

- [ ] **Step 2: Write the failing tests**

Create `crates/sandcastle-dockerfile/src/parser.rs` containing only this test module for now:

```rust
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
```

Create `crates/sandcastle-dockerfile/src/line_parsers.rs` containing only:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_words_matches_buildkit_cases() {
        // From BuildKit's TestParseWords.
        let cases: &[(&str, &[&str])] = &[
            ("foo", &["foo"]),
            ("foo bar", &["foo", "bar"]),
            ("foo\\ bar", &["foo\\ bar"]),
            ("foo=bar", &["foo=bar"]),
            ("foo bar 'abc xyz'", &["foo", "bar", "'abc xyz'"]),
            ("foo bar \"abc xyz\"", &["foo", "bar", "\"abc xyz\""]),
            ("àöû", &["àöû"]),
            ("föo bàr \"âbc xÿz\"", &["föo", "bàr", "\"âbc xÿz\""]),
        ];
        for (input, expected) in cases {
            assert_eq!(parse_words(input, '\\'), *expected, "{input:?}");
        }
    }

    #[test]
    fn flags_stop_at_double_dash_and_keep_unicode() {
        assert_eq!(
            extract_builder_flags("--a=1 --b -- --c x", '\\'),
            (" --c x", vec!["--a=1".into(), "--b".into()])
        );
        assert_eq!(
            extract_builder_flags("--chown=jürgen src dst", '\\'),
            ("src dst", vec!["--chown=jürgen".into()])
        );
        assert_eq!(
            extract_builder_flags("--x=\"a b\" y", '\\'),
            ("y", vec!["--x=a b".into()])
        );
        assert_eq!(extract_builder_flags("-x y", '\\'), ("-x y", vec![]));
    }

    #[test]
    fn env_without_value_is_an_error() {
        assert_eq!(
            name_val("PATH", "ENV", '\\'),
            Err(ParseErrorKind::MissingValue("ENV"))
        );
        assert_eq!(
            name_val("a=1 b", "ENV", '\\'),
            Err(ParseErrorKind::MissingEquals("b".into()))
        );
    }
}
```

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo test -p sandcastle-dockerfile`
Expected: FAIL to compile ("cannot find function `parse`", "cannot find type `Node`", "cannot find function `parse_words`" and similar) — the modules contain only tests.

- [ ] **Step 4: Implement the parser**

Prepend to `crates/sandcastle-dockerfile/src/parser.rs` (above the test module):

```rust
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
```

Prepend to `crates/sandcastle-dockerfile/src/line_parsers.rs`:

```rust
//! Per-instruction argument splitting, ported from BuildKit's
//! `frontend/dockerfile/parser/line_parsers.go` and `split_command.go`.

use crate::parser::{Node, ParseErrorKind};

/// Go `regexp` `[\t\v\f\r ]` (BuildKit's `reWhitespace`).
fn is_go_blank(c: char) -> bool {
    matches!(c, '\t' | '\x0b' | '\x0c' | '\r' | ' ')
}

/// `reWhitespace.Split(s, 2)`.
fn split_once_blank(s: &str) -> (&str, Option<&str>) {
    match s.find(is_go_blank) {
        Some(i) => (&s[..i], Some(s[i..].trim_start_matches(is_go_blank))),
        None => (s, None),
    }
}

/// Parses one logical line into a node (line numbers are filled in by the caller).
pub(crate) fn node_from_line(line: &str, escape: char) -> Result<Node, ParseErrorKind> {
    let (cmd, flags, rest) = split_command(line, escape);
    let mut node = Node {
        cmd: cmd.to_owned(),
        flags,
        args: Vec::new(),
        sub: None,
        json: false,
        original: line.to_owned(),
        start_line: 0,
        end_line: 0,
    };
    match cmd.to_ascii_lowercase().as_str() {
        "add" | "copy" | "volume" => (node.args, node.json) = maybe_json_to_list(rest)?,
        "cmd" | "entrypoint" | "run" | "shell" => (node.args, node.json) = maybe_json(rest)?,
        "arg" => node.args = parse_words(rest, escape),
        "env" => node.args = name_val(rest, "ENV", escape)?,
        "label" => node.args = name_val(rest, "LABEL", escape)?,
        "expose" | "from" => node.args = split_blank(rest),
        "maintainer" | "stopsignal" | "user" | "workdir" => node.args = single(rest),
        "healthcheck" => (node.args, node.json) = health_config(rest)?,
        "onbuild" if !rest.is_empty() => node.sub = Some(Box::new(node_from_line(rest, escape)?)),
        "onbuild" => {}
        // BuildKit keeps unknown instructions with one empty argument and
        // leaves rejecting them to the typed layer.
        _ => node.args = vec![String::new()],
    }
    Ok(node)
}

/// Splits `CMD --flag=x args` into the command, its builder flags and the
/// remaining argument text.
fn split_command(line: &str, escape: char) -> (&str, Vec<String>, &str) {
    let (cmd, rest) = split_once_blank(line.trim());
    match rest {
        Some(rest) => {
            let (args, flags) = extract_builder_flags(rest, escape);
            (cmd, flags, args.trim())
        }
        None => (cmd, Vec::new(), ""),
    }
}

/// Collects leading `--name[=value]` words; quotes group, the escape
/// character escapes the next character, and a bare `--` ends the flags.
///
/// BuildKit walks bytes here; this walks chars, so non-ASCII flag values
/// stay intact instead of being re-encoded byte by byte.
fn extract_builder_flags(line: &str, escape: char) -> (&str, Vec<String>) {
    #[derive(PartialEq)]
    enum Phase {
        Spaces,
        Word,
        Quote(char),
    }
    let mut words = Vec::new();
    let mut word = String::new();
    let mut blank_ok = false;
    let mut phase = Phase::Spaces;
    let mut chars = line.char_indices().peekable();

    while let Some((pos, ch)) = chars.next() {
        match phase {
            Phase::Spaces => {
                if ch.is_whitespace() {
                    continue;
                }
                if ch != '-' || chars.peek().map(|&(_, c)| c) != Some('-') {
                    return (&line[pos..], words);
                }
                phase = Phase::Word;
                word.push(ch);
            }
            Phase::Word => {
                if ch.is_whitespace() {
                    if word == "--" {
                        return (&line[pos..], words);
                    }
                    if blank_ok || !word.is_empty() {
                        words.push(std::mem::take(&mut word));
                    }
                    blank_ok = false;
                    phase = Phase::Spaces;
                } else if ch == '\'' || ch == '"' {
                    blank_ok = true;
                    phase = Phase::Quote(ch);
                } else if ch == escape {
                    if let Some((_, next)) = chars.next() {
                        word.push(next);
                    }
                } else {
                    word.push(ch);
                }
            }
            Phase::Quote(quote) => {
                if ch == quote {
                    phase = Phase::Word;
                } else if ch == escape {
                    match chars.next() {
                        Some((_, next)) => word.push(next),
                        None => phase = Phase::Word,
                    }
                } else {
                    word.push(ch);
                }
            }
        }
    }
    if phase != Phase::Spaces && word != "--" && (blank_ok || !word.is_empty()) {
        words.push(word);
    }
    ("", words)
}

/// Splits on whitespace while keeping quoted sections (quotes and escapes
/// stay in the words; expansion removes them later).
pub(crate) fn parse_words(rest: &str, escape: char) -> Vec<String> {
    #[derive(PartialEq)]
    enum Phase {
        Spaces,
        Word,
        Quote(char),
    }
    let mut words = Vec::new();
    let mut word = String::new();
    let mut blank_ok = false;
    let mut phase = Phase::Spaces;
    let mut chars = rest.chars().peekable();

    while let Some(mut ch) = chars.next() {
        if phase == Phase::Spaces {
            if ch.is_whitespace() {
                continue;
            }
            phase = Phase::Word;
        }
        match phase {
            Phase::Word => {
                if ch.is_whitespace() {
                    if blank_ok || !word.is_empty() {
                        words.push(std::mem::take(&mut word));
                    }
                    blank_ok = false;
                    phase = Phase::Spaces;
                    continue;
                }
                if ch == '\'' || ch == '"' {
                    blank_ok = true;
                    phase = Phase::Quote(ch);
                }
                if ch == escape {
                    let Some(next) = chars.next() else { continue };
                    word.push(ch);
                    ch = next;
                }
                word.push(ch);
            }
            Phase::Quote(quote) => {
                if ch == quote {
                    phase = Phase::Word;
                }
                if ch == escape && quote != '\'' {
                    let Some(next) = chars.next() else {
                        phase = Phase::Word;
                        continue;
                    };
                    word.push(ch);
                    ch = next;
                }
                word.push(ch);
            }
            Phase::Spaces => unreachable!("handled above"),
        }
    }
    if phase != Phase::Spaces && (blank_ok || !word.is_empty()) {
        words.push(word);
    }
    words
}

/// `ENV`/`LABEL`: `key value` (legacy, one pair) or `k1=v1 k2=v2`.
/// Produces `[key, value, sep]` triples with `sep` `""` (legacy) or `"="`.
fn name_val(rest: &str, cmd: &'static str, escape: char) -> Result<Vec<String>, ParseErrorKind> {
    let words = parse_words(rest, escape);
    let Some(first) = words.first() else {
        return Ok(Vec::new());
    };
    if !first.contains('=') {
        let (key, value) = split_once_blank(rest);
        let value = value.ok_or(ParseErrorKind::MissingValue(cmd))?;
        return Ok(vec![key.to_owned(), value.to_owned(), String::new()]);
    }
    let mut args = Vec::with_capacity(words.len() * 3);
    for word in words {
        let (key, value) = word
            .split_once('=')
            .ok_or_else(|| ParseErrorKind::MissingEquals(word.clone()))?;
        args.extend([key.to_owned(), value.to_owned(), "=".to_owned()]);
    }
    Ok(args)
}

fn split_blank(rest: &str) -> Vec<String> {
    if rest.is_empty() {
        return Vec::new();
    }
    rest.split(is_go_blank)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

fn single(rest: &str) -> Vec<String> {
    if rest.is_empty() {
        Vec::new()
    } else {
        vec![rest.to_owned()]
    }
}

enum Json {
    Array(Vec<String>),
    NotJson,
}

fn parse_json(rest: &str) -> Result<Json, ParseErrorKind> {
    let rest = rest.trim_start();
    if !rest.starts_with('[') {
        return Ok(Json::NotJson);
    }
    let Ok(values) = serde_json::from_str::<Vec<serde_json::Value>>(rest) else {
        return Ok(Json::NotJson);
    };
    values
        .into_iter()
        .map(|v| match v {
            serde_json::Value::String(s) => Ok(s),
            _ => Err(ParseErrorKind::NotStringArray),
        })
        .collect::<Result<_, _>>()
        .map(Json::Array)
}

/// RUN/CMD/ENTRYPOINT/SHELL: exec form, or the whole text as one argument.
fn maybe_json(rest: &str) -> Result<(Vec<String>, bool), ParseErrorKind> {
    if rest.is_empty() {
        return Ok((Vec::new(), false));
    }
    Ok(match parse_json(rest)? {
        Json::Array(args) => (args, true),
        Json::NotJson => (vec![rest.to_owned()], false),
    })
}

/// ADD/COPY/VOLUME: exec form, or whitespace-separated words.
fn maybe_json_to_list(rest: &str) -> Result<(Vec<String>, bool), ParseErrorKind> {
    Ok(match parse_json(rest)? {
        Json::Array(args) => (args, true),
        Json::NotJson => (split_blank(rest), false),
    })
}

/// HEALTHCHECK: a type word (`CMD`, `NONE`, ...) then RUN-style arguments.
fn health_config(rest: &str) -> Result<(Vec<String>, bool), ParseErrorKind> {
    let sep = rest.find(char::is_whitespace).unwrap_or(rest.len());
    if sep == 0 {
        return Ok((Vec::new(), false));
    }
    let (kind, cmd) = rest.split_at(sep);
    let (args, json) = maybe_json(cmd.trim_start())?;
    Ok((std::iter::once(kind.to_owned()).chain(args).collect(), json))
}
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p sandcastle-dockerfile`
Expected: PASS (11 tests).

Run: `cargo fmt --all --check && cargo clippy -p sandcastle-dockerfile --all-targets -- -D warnings`
Expected: no output, exit 0.

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml Cargo.lock crates/sandcastle-dockerfile
git commit -m "Add sandcastle-dockerfile crate with BuildKit-compatible parser" -m "Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 2: Vendored BuildKit parser fixtures

**Files:**
- Create: `crates/sandcastle-dockerfile/.gitattributes`, `crates/sandcastle-dockerfile/testdata/buildkit/**` (vendored), `crates/sandcastle-dockerfile/src/dump.rs`, `crates/sandcastle-dockerfile/src/buildkit_fixtures.rs`
- Modify: `crates/sandcastle-dockerfile/src/lib.rs`, `NOTICE`

**Interfaces:**
- Consumes: `parse`, `Node` (Task 1).
- Produces: test-only `dump::dump(&[Node]) -> String` (BuildKit `Node.Dump()` of the root, without the trailing newline); `buildkit_fixtures` helpers `testdata(rel: &str) -> PathBuf`, `case_dirs(rel: &str) -> Vec<PathBuf>`, used again in Task 4.

- [ ] **Step 1: Vendor the fixtures at the pinned commit**

From the repository root:

```bash
BK=$(mktemp -d)
git clone -q --filter=blob:none --no-checkout https://github.com/moby/buildkit "$BK"
git -C "$BK" sparse-checkout set --no-cone /LICENSE \
  /frontend/dockerfile/parser/testfiles /frontend/dockerfile/parser/testfiles-negative \
  /frontend/dockerfile/parser/testfile-line \
  /frontend/dockerfile/shell/envVarTest /frontend/dockerfile/shell/wordsTest
git -C "$BK" -c advice.detachedHead=false checkout -q 82a1f40db9624f03b94bc2a42f0e5bac0e89aad7
D=crates/sandcastle-dockerfile/testdata/buildkit
mkdir -p "$D/parser" "$D/shell"
cp -R "$BK"/frontend/dockerfile/parser/testfiles "$BK"/frontend/dockerfile/parser/testfiles-negative \
  "$BK"/frontend/dockerfile/parser/testfile-line "$D/parser/"
cp "$BK"/frontend/dockerfile/shell/envVarTest "$BK"/frontend/dockerfile/shell/wordsTest "$D/shell/"
cp "$BK"/LICENSE "$D/LICENSE"
rm -rf "$BK"
find "$D" -type f | wc -l
```

Expected: `74`. (`testfiles-negative/empty_dockerfile/Dockerfile` is an empty file and must be kept; several files have significant trailing whitespace.)

`crates/sandcastle-dockerfile/testdata/buildkit/SOURCE`:

```
https://github.com/moby/buildkit 82a1f40db9624f03b94bc2a42f0e5bac0e89aad7
frontend/dockerfile/parser/testfiles
frontend/dockerfile/parser/testfiles-negative
frontend/dockerfile/parser/testfile-line
frontend/dockerfile/shell/envVarTest
frontend/dockerfile/shell/wordsTest
```

`crates/sandcastle-dockerfile/.gitattributes`:

```
# Vendored BuildKit fixtures are compared byte for byte.
testdata/** -text
```

Append to `NOTICE`:

```

The sandcastle-dockerfile crate ports BuildKit's frontend/dockerfile parser,
instructions and shell packages (https://github.com/moby/buildkit,
Apache-2.0). Its tests vendor BuildKit fixtures under
crates/sandcastle-dockerfile/testdata/buildkit/ with BuildKit's LICENSE; the
pinned commit is in testdata/buildkit/SOURCE.
```

- [ ] **Step 2: Write the failing fixture tests**

Replace `crates/sandcastle-dockerfile/src/lib.rs` with:

```rust
//! Dockerfile parsing for sandcastle.
//!
//! `parse` turns Dockerfile text into `Node`s with the same rules as
//! BuildKit's `frontend/dockerfile/parser`; `Instruction::try_from` gives the
//! typed form of one node, and `expand` / `expand_words` perform BuildKit's
//! variable expansion and quote removal on an argument. The crate does no I/O.

#![forbid(unsafe_code)]

mod line_parsers;
mod parser;

#[cfg(test)]
mod buildkit_fixtures;
#[cfg(test)]
mod dump;

pub use parser::{Dockerfile, Node, ParseError, ParseErrorKind, parse};
```

`crates/sandcastle-dockerfile/src/buildkit_fixtures.rs`:

```rust
//! BuildKit's own parser and shell-lexer fixtures, vendored under
//! `testdata/buildkit/` (see `testdata/buildkit/SOURCE`), as table tests.

use std::fs;
use std::path::{Path, PathBuf};

use crate::dump::dump;
use crate::parse;

fn testdata(rel: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("testdata/buildkit")
        .join(rel)
}

fn case_dirs(rel: &str) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = fs::read_dir(testdata(rel))
        .unwrap_or_else(|e| panic!("{rel}: {e}"))
        .map(|e| e.expect("dir entry").path())
        .collect();
    dirs.sort();
    assert!(!dirs.is_empty(), "no fixtures in {rel}");
    dirs
}

#[test]
fn parser_testfiles_match_buildkit_dump() {
    let mut failures = Vec::new();
    for dir in case_dirs("parser/testfiles") {
        let src = fs::read_to_string(dir.join("Dockerfile")).expect("Dockerfile");
        let expected = fs::read_to_string(dir.join("result")).expect("result");
        match parse(&src) {
            Ok(df) if dump(&df.nodes) + "\n" == expected => {}
            Ok(df) => failures.push(format!(
                "{}:\n--- expected\n{expected}--- got\n{}\n",
                dir.display(),
                dump(&df.nodes)
            )),
            Err(e) => failures.push(format!("{}: {e}", dir.display())),
        }
    }
    assert!(
        failures.is_empty(),
        "{} fixture(s) differ:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
fn parser_negative_testfiles_fail() {
    for dir in case_dirs("parser/testfiles-negative") {
        let src = fs::read_to_string(dir.join("Dockerfile")).expect("Dockerfile");
        assert!(
            parse(&src).is_err(),
            "{} parsed but must fail",
            dir.display()
        );
    }
}

#[test]
fn parser_line_numbers_match_buildkit() {
    let src = fs::read_to_string(testdata("parser/testfile-line/Dockerfile")).expect("Dockerfile");
    let df = parse(&src).expect("parses");
    let ranges: Vec<_> = df
        .nodes
        .iter()
        .map(|n| (n.start_line, n.end_line))
        .collect();
    assert_eq!(ranges, [(5, 5), (11, 12), (17, 31)]);
}
```

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo test -p sandcastle-dockerfile`
Expected: FAIL to compile with "file not found for module `dump`" / "unresolved import `crate::dump`".

- [ ] **Step 4: Implement the dump**

`crates/sandcastle-dockerfile/src/dump.rs`:

```rust
//! Test-only rendering of nodes in BuildKit's `Node.Dump()` format, so the
//! vendored BuildKit `result` files can be compared byte for byte.

use std::fmt::Write;

use crate::parser::Node;

/// All nodes, one `(...)` per line, like dumping BuildKit's root node.
pub(crate) fn dump(nodes: &[Node]) -> String {
    nodes
        .iter()
        .map(|n| format!("({})", dump_node(n)))
        .collect::<Vec<_>>()
        .join("\n")
}

fn dump_node(node: &Node) -> String {
    let mut out = node.cmd.to_lowercase();
    if !node.flags.is_empty() {
        let flags: Vec<String> = node.flags.iter().map(|f| go_quote(f)).collect();
        write!(out, " [{}]", flags.join(" ")).expect("writing to a String");
    }
    for arg in &node.args {
        out.push(' ');
        out.push_str(&go_quote(arg));
    }
    if let Some(sub) = &node.sub {
        write!(out, " ({})", dump_node(sub)).expect("writing to a String");
    }
    out
}

/// Go's `strconv.Quote`. Go keeps "printable" runes (Unicode L, M, N, P, S
/// and ASCII space) as-is; std has no category tables, so this treats every
/// non-control, non-whitespace rune as printable, which is exact for the
/// vendored fixtures.
fn go_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\x07' => out.push_str("\\a"),
            '\x08' => out.push_str("\\b"),
            '\x0c' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\x0b' => out.push_str("\\v"),
            ' ' => out.push(' '),
            c if c.is_ascii_control() => {
                write!(out, "\\x{:02x}", c as u32).expect("writing to a String")
            }
            c if c.is_control() || c.is_whitespace() => {
                if (c as u32) < 0x10000 {
                    write!(out, "\\u{:04x}", c as u32)
                } else {
                    write!(out, "\\U{:08x}", c as u32)
                }
                .expect("writing to a String");
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p sandcastle-dockerfile`
Expected: PASS (14 tests), including `parser_testfiles_match_buildkit_dump` over all 33 fixture directories. If a fixture differs, the assertion prints expected vs. got per directory: fix the parser (Task 1 code), never the fixture.

Run: `cargo fmt --all --check && cargo clippy -p sandcastle-dockerfile --all-targets -- -D warnings`
Expected: exit 0.

- [ ] **Step 6: Commit**

```bash
git add NOTICE crates/sandcastle-dockerfile
git commit -m "Run BuildKit parser fixtures against sandcastle-dockerfile" -m "Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 3: Typed instructions

**Files:**
- Create: `crates/sandcastle-dockerfile/src/instruction.rs`
- Modify: `crates/sandcastle-dockerfile/src/lib.rs`

**Interfaces:**
- Consumes: `Node` conventions from Task 1.
- Produces: `Instruction` (`From { image, stage: Option<String>, flags }`, `Run { command, flags }`, `Copy { sources, dest, flags }`, `Env(Vec<KeyValue>)`, `Label(Vec<KeyValue>)`, `Workdir(String)`, `User(String)`, `Expose(Vec<String>)` sorted, `Cmd(Command)`, `Entrypoint(Command)`, `Other(Node)`); `impl TryFrom<&Node> for Instruction`; `Command::{Shell(String), Exec(Vec<String>)}`; `Flag { name: String, value: String }` (no `--`, bare bool = `"true"`); `KeyValue { key, value }`; `InstructionError::{line(), kind()}`; `InstructionErrorKind` (`#[non_exhaustive]`). Arguments stay unexpanded.

- [ ] **Step 1: Write the failing tests**

Replace `crates/sandcastle-dockerfile/src/lib.rs` with:

```rust
//! Dockerfile parsing for sandcastle.
//!
//! `parse` turns Dockerfile text into `Node`s with the same rules as
//! BuildKit's `frontend/dockerfile/parser`; `Instruction::try_from` gives the
//! typed form of one node, and `expand` / `expand_words` perform BuildKit's
//! variable expansion and quote removal on an argument. The crate does no I/O.

#![forbid(unsafe_code)]

mod instruction;
mod line_parsers;
mod parser;

#[cfg(test)]
mod buildkit_fixtures;
#[cfg(test)]
mod dump;

pub use instruction::{
    Command, Flag, Instruction, InstructionError, InstructionErrorKind, KeyValue,
};
pub use parser::{Dockerfile, Node, ParseError, ParseErrorKind, parse};
```

Create `crates/sandcastle-dockerfile/src/instruction.rs` containing only:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse;

    fn one(src: &str) -> Result<Instruction, InstructionError> {
        let df = parse(src).expect("parses");
        Instruction::try_from(df.nodes.last().expect("one node"))
    }

    fn flag(name: &str, value: &str) -> Flag {
        Flag {
            name: name.into(),
            value: value.into(),
        }
    }

    #[test]
    fn from_with_stage_and_platform() {
        assert_eq!(
            one("FROM --platform=linux/arm64 alpine:3.20 AS Build").unwrap(),
            Instruction::From {
                image: "alpine:3.20".into(),
                stage: Some("build".into()),
                flags: vec![flag("platform", "linux/arm64")]
            }
        );
        assert_eq!(
            one("FROM a b").unwrap_err().kind(),
            &InstructionErrorKind::FromArguments
        );
        assert_eq!(
            one("FROM a AS 1x").unwrap_err().kind(),
            &InstructionErrorKind::InvalidStageName("1x".into())
        );
    }

    #[test]
    fn run_cmd_entrypoint_shell_and_exec_forms() {
        assert_eq!(
            one("RUN apt-get update && echo \"hi\"").unwrap(),
            Instruction::Run {
                command: Command::Shell("apt-get update && echo \"hi\"".into()),
                flags: vec![]
            }
        );
        assert_eq!(
            one("CMD [\"a\", \"b c\"]").unwrap(),
            Instruction::Cmd(Command::Exec(vec!["a".into(), "b c".into()]))
        );
        assert_eq!(
            one("ENTRYPOINT []").unwrap(),
            Instruction::Entrypoint(Command::Exec(vec![]))
        );
        assert_eq!(
            one("CMD").unwrap(),
            Instruction::Cmd(Command::Shell(String::new()))
        );
    }

    #[test]
    fn copy_needs_a_destination_and_known_flags() {
        assert_eq!(
            one("COPY --link --chown=1:1 a b /dst/").unwrap(),
            Instruction::Copy {
                sources: vec!["a".into(), "b".into()],
                dest: "/dst/".into(),
                flags: vec![flag("link", "true"), flag("chown", "1:1")]
            }
        );
        assert_eq!(
            one("COPY [\"a b\", \"/c\"]").unwrap(),
            Instruction::Copy {
                sources: vec!["a b".into()],
                dest: "/c".into(),
                flags: vec![]
            }
        );
        assert_eq!(
            one("COPY a").unwrap_err().kind(),
            &InstructionErrorKind::NoDestination("COPY")
        );
        assert_eq!(
            one("COPY --mode=1 a b").unwrap_err().kind(),
            &InstructionErrorKind::UnknownFlag("--mode".into())
        );
        assert_eq!(
            one("COPY --from a b").unwrap_err().kind(),
            &InstructionErrorKind::MissingFlagValue("--from".into())
        );
        assert_eq!(
            one("COPY --link=maybe a b").unwrap_err().kind(),
            &InstructionErrorKind::NotBoolean {
                flag: "--link".into(),
                value: "maybe".into()
            }
        );
        assert_eq!(
            one("COPY --chown=1 --chown=2 a b").unwrap_err().kind(),
            &InstructionErrorKind::DuplicateFlag("--chown".into())
        );
    }

    #[test]
    fn env_and_label_pairs_stay_unexpanded() {
        assert_eq!(
            one("ENV A=\"x y\" B=$A").unwrap(),
            Instruction::Env(vec![
                KeyValue {
                    key: "A".into(),
                    value: "\"x y\"".into()
                },
                KeyValue {
                    key: "B".into(),
                    value: "$A".into()
                },
            ])
        );
        assert_eq!(
            one("LABEL k some value").unwrap(),
            Instruction::Label(vec![KeyValue {
                key: "k".into(),
                value: "some value".into()
            }])
        );
        assert_eq!(
            one("ENV =x").unwrap_err().kind(),
            &InstructionErrorKind::BlankName("ENV")
        );
        assert_eq!(
            one("ENV").unwrap_err().kind(),
            &InstructionErrorKind::AtLeastOneArgument("ENV")
        );
        assert_eq!(
            one("ENV --x=1 A=b").unwrap_err().kind(),
            &InstructionErrorKind::UnknownFlag("--x".into())
        );
    }

    #[test]
    fn single_argument_instructions() {
        assert_eq!(
            one("WORKDIR /a b").unwrap(),
            Instruction::Workdir("/a b".into())
        );
        assert_eq!(one("USER 0").unwrap(), Instruction::User("0".into()));
        assert_eq!(
            one("USER").unwrap_err().kind(),
            &InstructionErrorKind::ExactlyOneArgument("USER")
        );
        assert_eq!(
            one("EXPOSE 80 443/tcp 8080").unwrap(),
            Instruction::Expose(vec!["443/tcp".into(), "80".into(), "8080".into()])
        );
        assert_eq!(
            one("EXPOSE").unwrap_err().kind(),
            &InstructionErrorKind::AtLeastOneArgument("EXPOSE")
        );
    }

    #[test]
    fn other_and_unknown_instructions() {
        assert!(matches!(one("VOLUME /data").unwrap(), Instruction::Other(n) if n.cmd == "VOLUME"));
        assert!(matches!(
            one("onbuild run x").unwrap(),
            Instruction::Other(_)
        ));
        let err = one("FROM a\nFROOM b").unwrap_err();
        assert_eq!(
            err.kind(),
            &InstructionErrorKind::UnknownInstruction("FROOM".into())
        );
        assert_eq!(err.line(), 2);
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p sandcastle-dockerfile instruction`
Expected: FAIL to compile ("cannot find type `Instruction`").

- [ ] **Step 3: Implement**

Prepend to `crates/sandcastle-dockerfile/src/instruction.rs`:

```rust
//! Typed instructions, following the argument and flag rules of BuildKit's
//! `frontend/dockerfile/instructions/parse.go`.

use std::fmt;

use crate::parser::Node;

/// A command line for RUN, CMD or ENTRYPOINT.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Shell form: run through the image's shell.
    Shell(String),
    /// Exec form (JSON array): argv as written.
    Exec(Vec<String>),
}

/// A builder flag, checked against the flags BuildKit accepts for the
/// instruction. `name` has no leading `--`; a bare boolean flag has value
/// `"true"`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Flag {
    pub name: String,
    pub value: String,
}

/// `key=value` from ENV or LABEL. Both sides are unexpanded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyValue {
    pub key: String,
    pub value: String,
}

/// A Dockerfile instruction. Arguments are unexpanded: apply
/// [`crate::expand`] where Docker expands variables.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Instruction {
    From {
        image: String,
        stage: Option<String>,
        flags: Vec<Flag>,
    },
    Run {
        command: Command,
        flags: Vec<Flag>,
    },
    Copy {
        sources: Vec<String>,
        dest: String,
        flags: Vec<Flag>,
    },
    Env(Vec<KeyValue>),
    Label(Vec<KeyValue>),
    Workdir(String),
    User(String),
    /// Ports sorted as BuildKit sorts them.
    Expose(Vec<String>),
    Cmd(Command),
    Entrypoint(Command),
    /// A valid instruction without a typed form here (ADD, ARG, HEALTHCHECK,
    /// MAINTAINER, ONBUILD, SHELL, STOPSIGNAL, VOLUME).
    Other(Node),
}

/// Why a node is not a valid instruction.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("line {line}: {kind}")]
pub struct InstructionError {
    line: usize,
    kind: InstructionErrorKind,
}

impl InstructionError {
    /// 1-based first line of the instruction.
    pub fn line(&self) -> usize {
        self.line
    }

    pub fn kind(&self) -> &InstructionErrorKind {
        &self.kind
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum InstructionErrorKind {
    UnknownInstruction(String),
    AtLeastOneArgument(&'static str),
    ExactlyOneArgument(&'static str),
    NoDestination(&'static str),
    FromArguments,
    InvalidStageName(String),
    BlankName(&'static str),
    UnknownFlag(String),
    DuplicateFlag(String),
    MissingFlagValue(String),
    NotBoolean { flag: String, value: String },
}

impl fmt::Display for InstructionErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownInstruction(cmd) => write!(f, "unknown instruction: {cmd}"),
            Self::AtLeastOneArgument(cmd) => write!(f, "{cmd} requires at least one argument"),
            Self::ExactlyOneArgument(cmd) => write!(f, "{cmd} requires exactly one argument"),
            Self::NoDestination(cmd) => write!(
                f,
                "{cmd} requires at least two arguments, but only one was provided. Destination could not be determined"
            ),
            Self::FromArguments => f.write_str("FROM requires either one or three arguments"),
            Self::InvalidStageName(name) => write!(
                f,
                "invalid name for build stage: {name:?}, name can't start with a number or contain symbols"
            ),
            Self::BlankName(cmd) => write!(f, "{cmd} names can not be blank"),
            Self::UnknownFlag(flag) => write!(f, "unknown flag: {flag}"),
            Self::DuplicateFlag(flag) => write!(f, "duplicate flag specified: {flag}"),
            Self::MissingFlagValue(flag) => write!(f, "missing a value on flag: {flag}"),
            Self::NotBoolean { flag, value } => {
                write!(f, "expecting boolean value for flag {flag}, not: {value}")
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum FlagKind {
    String,
    Bool,
    /// May repeat.
    Strings,
}

const FROM_FLAGS: &[(&str, FlagKind)] = &[("platform", FlagKind::String)];
const RUN_FLAGS: &[(&str, FlagKind)] = &[
    ("mount", FlagKind::Strings),
    ("network", FlagKind::String),
    ("security", FlagKind::String),
    ("device", FlagKind::Strings),
];
const COPY_FLAGS: &[(&str, FlagKind)] = &[
    ("chown", FlagKind::String),
    ("from", FlagKind::String),
    ("chmod", FlagKind::String),
    ("link", FlagKind::Bool),
    ("exclude", FlagKind::Strings),
    ("parents", FlagKind::Bool),
];

impl TryFrom<&Node> for Instruction {
    type Error = InstructionError;

    fn try_from(node: &Node) -> Result<Self, Self::Error> {
        typed(node).map_err(|kind| InstructionError {
            line: node.start_line,
            kind,
        })
    }
}

fn typed(node: &Node) -> Result<Instruction, InstructionErrorKind> {
    use InstructionErrorKind as E;
    let args = &node.args;
    Ok(match node.cmd.to_ascii_lowercase().as_str() {
        "from" => {
            let flags = parse_flags(&node.flags, FROM_FLAGS)?;
            let stage = match args.as_slice() {
                [_] => None,
                [_, as_kw, name] if as_kw.eq_ignore_ascii_case("as") => {
                    let stage = name.to_lowercase();
                    if !is_valid_stage_name(&stage) {
                        return Err(E::InvalidStageName(name.clone()));
                    }
                    Some(stage)
                }
                _ => return Err(E::FromArguments),
            };
            Instruction::From {
                image: args[0].clone(),
                stage,
                flags,
            }
        }
        "run" => Instruction::Run {
            flags: parse_flags(&node.flags, RUN_FLAGS)?,
            command: command(node),
        },
        "copy" => {
            let flags = parse_flags(&node.flags, COPY_FLAGS)?;
            let Some((dest, sources)) = args.split_last().filter(|(_, s)| !s.is_empty()) else {
                return Err(E::NoDestination("COPY"));
            };
            Instruction::Copy {
                sources: sources.to_vec(),
                dest: dest.clone(),
                flags,
            }
        }
        "env" => {
            parse_flags(&node.flags, &[])?;
            Instruction::Env(key_values(args, "ENV")?)
        }
        "label" => {
            parse_flags(&node.flags, &[])?;
            Instruction::Label(key_values(args, "LABEL")?)
        }
        "workdir" => Instruction::Workdir(exactly_one(node, "WORKDIR")?),
        "user" => Instruction::User(exactly_one(node, "USER")?),
        "expose" => {
            if args.is_empty() {
                return Err(E::AtLeastOneArgument("EXPOSE"));
            }
            parse_flags(&node.flags, &[])?;
            let mut ports = args.clone();
            ports.sort();
            Instruction::Expose(ports)
        }
        "cmd" => {
            parse_flags(&node.flags, &[])?;
            Instruction::Cmd(command(node))
        }
        "entrypoint" => {
            parse_flags(&node.flags, &[])?;
            Instruction::Entrypoint(command(node))
        }
        "add" | "arg" | "healthcheck" | "maintainer" | "onbuild" | "shell" | "stopsignal"
        | "volume" => Instruction::Other(node.clone()),
        _ => return Err(E::UnknownInstruction(node.cmd.clone())),
    })
}

fn command(node: &Node) -> Command {
    if node.json {
        Command::Exec(node.args.clone())
    } else {
        Command::Shell(node.args.join(" "))
    }
}

fn exactly_one(node: &Node, cmd: &'static str) -> Result<String, InstructionErrorKind> {
    let [arg] = node.args.as_slice() else {
        return Err(InstructionErrorKind::ExactlyOneArgument(cmd));
    };
    parse_flags(&node.flags, &[])?;
    Ok(arg.clone())
}

/// `[key, value, sep]` triples from the parser.
fn key_values(args: &[String], cmd: &'static str) -> Result<Vec<KeyValue>, InstructionErrorKind> {
    if args.is_empty() {
        return Err(InstructionErrorKind::AtLeastOneArgument(cmd));
    }
    args.chunks(3)
        .map(|kv| match kv {
            [key, ..] if key.is_empty() => Err(InstructionErrorKind::BlankName(cmd)),
            [key, value, _] => Ok(KeyValue {
                key: key.clone(),
                value: value.clone(),
            }),
            _ => unreachable!("the parser emits complete key/value/separator triples"),
        })
        .collect()
}

/// BuildKit's `^[a-z][a-z0-9-_.]*$`.
fn is_valid_stage_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && chars
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_' | '.'))
}

/// BuildKit's `BFlags.Parse`.
fn parse_flags(
    raw: &[String],
    allowed: &[(&str, FlagKind)],
) -> Result<Vec<Flag>, InstructionErrorKind> {
    use InstructionErrorKind as E;
    let mut flags: Vec<Flag> = Vec::with_capacity(raw.len());
    for arg in raw {
        let (flag_name, value) = match arg.split_once('=') {
            Some((n, v)) => (n, Some(v)),
            None => (arg.as_str(), None),
        };
        let name = flag_name.strip_prefix("--").unwrap_or(flag_name);
        let Some(&(_, kind)) = allowed.iter().find(|(n, _)| *n == name) else {
            return Err(E::UnknownFlag(flag_name.to_owned()));
        };
        if kind != FlagKind::Strings && flags.iter().any(|f| f.name == name) {
            return Err(E::DuplicateFlag(flag_name.to_owned()));
        }
        let value = match (kind, value) {
            (FlagKind::Bool, Some("")) | (FlagKind::String | FlagKind::Strings, None) => {
                return Err(E::MissingFlagValue(flag_name.to_owned()));
            }
            (FlagKind::Bool, None) => "true".to_owned(),
            (FlagKind::Bool, Some(v)) => match v.to_ascii_lowercase().as_str() {
                "true" => "true".to_owned(),
                "false" => "false".to_owned(),
                _ => {
                    return Err(E::NotBoolean {
                        flag: flag_name.to_owned(),
                        value: v.to_owned(),
                    });
                }
            },
            (_, Some(v)) => v.to_owned(),
        };
        flags.push(Flag {
            name: name.to_owned(),
            value,
        });
    }
    Ok(flags)
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p sandcastle-dockerfile`
Expected: PASS (20 tests).

Run: `cargo fmt --all --check && cargo clippy -p sandcastle-dockerfile --all-targets -- -D warnings`
Expected: exit 0.

- [ ] **Step 5: Commit**

```bash
git add crates/sandcastle-dockerfile
git commit -m "Type v1 Dockerfile instructions with BuildKit argument and flag rules" -m "Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 4: Variable expansion

**Files:**
- Create: `crates/sandcastle-dockerfile/src/expand.rs`
- Modify: `crates/sandcastle-dockerfile/src/lib.rs`, `crates/sandcastle-dockerfile/src/buildkit_fixtures.rs`

**Interfaces:**
- Consumes: vendored `testdata/buildkit/shell/{envVarTest,wordsTest}` (Task 2), `testdata()` helper.
- Produces: `trait Env { fn get(&self, name: &str) -> Option<&str>; }` implemented for `HashMap<String, String, S>`; `expand(word, &impl Env, escape) -> Result<String, ExpandError>` (BuildKit `ProcessWord`); `expand_words(word, &impl Env, escape) -> Result<Vec<String>, ExpandError>` (BuildKit `ProcessWords`); `ExpandError::word()`, `Display` = BuildKit's message.

- [ ] **Step 1: Write the failing tests**

Replace `crates/sandcastle-dockerfile/src/lib.rs` with:

```rust
//! Dockerfile parsing for sandcastle.
//!
//! `parse` turns Dockerfile text into `Node`s with the same rules as
//! BuildKit's `frontend/dockerfile/parser`; `Instruction::try_from` gives the
//! typed form of one node, and `expand` / `expand_words` perform BuildKit's
//! variable expansion and quote removal on an argument. The crate does no I/O.

#![forbid(unsafe_code)]

mod expand;
mod instruction;
mod line_parsers;
mod parser;

#[cfg(test)]
mod buildkit_fixtures;
#[cfg(test)]
mod dump;

pub use expand::{Env, ExpandError, expand, expand_words};
pub use instruction::{
    Command, Flag, Instruction, InstructionError, InstructionErrorKind, KeyValue,
};
pub use parser::{Dockerfile, Node, ParseError, ParseErrorKind, parse};
```

In `crates/sandcastle-dockerfile/src/buildkit_fixtures.rs`, change the imports to:

```rust
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::dump::dump;
use crate::{expand, expand_words, parse};
```

and append:

```rust
/// `envVarTest`: `platform | input | expected` with platform A (all), U
/// (unix) or W (windows, skipped); expected `error` means expansion fails.
#[test]
fn shell_env_var_test_matches_buildkit() {
    let env: HashMap<String, String> = [
        ("PWD", "/home"),
        ("SHELL", "bash"),
        ("KOREAN", "한국어"),
        ("NULL", ""),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v.to_owned()))
    .collect();
    let data = fs::read_to_string(testdata("shell/envVarTest")).expect("envVarTest");
    let mut checked = 0;
    for (n, line) in data.lines().enumerate() {
        if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.trim().split('|').collect();
        assert_eq!(fields.len(), 3, "line {}", n + 1);
        let (platform, input, expected) = (fields[0].trim(), fields[1].trim(), fields[2].trim());
        if platform == "W" {
            continue;
        }
        let got = expand(input, &env, '\\');
        if expected == "error" {
            assert!(
                got.is_err(),
                "line {}: {input:?} gave {got:?}, want error",
                n + 1
            );
        } else {
            assert_eq!(got.as_deref(), Ok(expected), "line {}: {input:?}", n + 1);
        }
        checked += 1;
    }
    assert!(checked > 200, "only {checked} cases ran");
}

/// `wordsTest`: `ENV k=v` lines extend the environment; `input | w1,w2`
/// lines expect those words (or `error`).
#[test]
fn shell_words_test_matches_buildkit() {
    let data = fs::read_to_string(testdata("shell/wordsTest")).expect("wordsTest");
    let mut env = HashMap::new();
    for (n, line) in data.lines().enumerate() {
        if line.starts_with('#') {
            continue;
        }
        if let Some(assignment) = line.strip_prefix("ENV ") {
            let (k, v) = assignment
                .trim_start_matches(' ')
                .split_once('=')
                .expect("k=v");
            env.insert(k.to_owned(), v.to_owned());
            continue;
        }
        let (input, expected) = line
            .split_once('|')
            .unwrap_or_else(|| panic!("line {}: no |", n + 1));
        let expected: Vec<&str> = expected.trim_start_matches(' ').split(',').collect();
        let got =
            expand_words(input.trim(), &env, '\\').unwrap_or_else(|_| vec!["error".to_owned()]);
        assert_eq!(got, expected, "line {}: {input:?}", n + 1);
    }
}
```

Create `crates/sandcastle-dockerfile/src/expand.rs` containing only:

```rust
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
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p sandcastle-dockerfile`
Expected: FAIL to compile ("cannot find function `expand`", "cannot find type `HashMap`" in the `expand.rs` tests).

- [ ] **Step 3: Implement**

Prepend to `crates/sandcastle-dockerfile/src/expand.rs`:

```rust
//! Variable expansion and quote removal for instruction arguments, ported
//! from BuildKit's `frontend/dockerfile/shell/lex.go` (default mode).
//!
//! Supported: `$VAR`, `${VAR}`, `${VAR-w}`, `${VAR:-w}`, `${VAR+w}`,
//! `${VAR:+w}`, `${VAR?msg}`, `${VAR:?msg}`, single and double quotes, and
//! the escape character. Pattern operators (`#`, `%`, `/`) are rejected.

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
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ExpandErrorKind {
    UnterminatedSingleQuote,
    UnterminatedDoubleQuote,
    Unterminated(char),
    MissingBrace,
    BadSubstitution,
    UnsupportedModifier(String),
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
        let (word, _) = self.until(Some('}')).map_err(|e| match e {
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
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p sandcastle-dockerfile`
Expected: PASS (26 tests); `shell_env_var_test_matches_buildkit` checks more than 200 lines of `envVarTest`.

Run: `cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`
Expected: exit 0; the rest of the workspace is unaffected.

- [ ] **Step 5: Commit**

```bash
git add crates/sandcastle-dockerfile
git commit -m "Add BuildKit-compatible variable expansion to sandcastle-dockerfile" -m "Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

## Out of scope for this plan

Heredocs (rejected with `ParseErrorKind::Heredoc`), typed forms for ADD/ARG/HEALTHCHECK/ONBUILD/SHELL/STOPSIGNAL/VOLUME/MAINTAINER (`Instruction::Other`), pattern operators in expansion, parser warnings, multi-stage semantics, and the host `dockerfile` adapter that rejects out-of-scope instructions (plan 4).
