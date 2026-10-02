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
