//! BuildKit's own parser and shell-lexer fixtures, vendored under
//! `testdata/buildkit/` (see `testdata/buildkit/SOURCE`), as table tests.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::dump::dump;
use crate::{expand, expand_words, parse};

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
