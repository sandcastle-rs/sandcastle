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
