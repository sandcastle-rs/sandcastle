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
    parse_line(line, escape, false)
}

/// `in_onbuild` is set for an ONBUILD trigger, which may not be ONBUILD again;
/// this keeps the recursion at most one level deep.
fn parse_line(line: &str, escape: char, in_onbuild: bool) -> Result<Node, ParseErrorKind> {
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
        "onbuild" if in_onbuild => return Err(ParseErrorKind::ChainedOnbuild),
        "onbuild" if !rest.is_empty() => node.sub = Some(Box::new(parse_line(rest, escape, true)?)),
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
