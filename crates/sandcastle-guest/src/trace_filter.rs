//! Separates `sh -x` trace lines from a RUN step's stderr. The shell runs
//! with `PS4` set to a per-job marker; from a marker to the end of its line
//! is the command the shell is about to run. A marker may follow output
//! that had no trailing newline. Everything else is forwarded as it
//! arrives, holding back at most a possible marker prefix.

use sandcastle_proto::MAX_CMD_TEXT;

/// Longest run of `+` accepted before the marker (bash repeats PS4's first
/// character once per nesting level).
const MAX_PLUS: usize = 64;

pub struct MarkerFilter {
    /// The marker after the leading `+` run: `sc-<token>> `.
    tail: Vec<u8>,
    /// Inside a marker line; bytes go to `cmd`.
    capturing: bool,
    /// Bytes held back while they may still begin a marker; always empty
    /// or starting with `+`.
    pending: Vec<u8>,
    /// Command text of the marker line being read.
    cmd: Vec<u8>,
    truncated: u64,
}

impl MarkerFilter {
    pub fn new(token: &str) -> Self {
        Self {
            tail: format!("sc-{token}> ").into_bytes(),
            capturing: false,
            pending: Vec::new(),
            cmd: Vec::new(),
            truncated: 0,
        }
    }

    /// The value for the shell's `PS4`.
    pub fn ps4(&self) -> String {
        format!("+{}", String::from_utf8_lossy(&self.tail))
    }

    pub fn truncated(&self) -> u64 {
        self.truncated
    }

    pub fn feed(&mut self, chunk: &[u8], out: &mut Vec<u8>, cmds: &mut Vec<String>) {
        for &b in chunk {
            if self.capturing {
                if b == b'\n' {
                    self.emit(cmds);
                    self.capturing = false;
                } else if self.cmd.len() <= MAX_CMD_TEXT * 4 {
                    self.cmd.push(b);
                }
            } else if self.pending.is_empty() && b != b'+' {
                out.push(b);
            } else {
                self.pending.push(b);
                self.settle(out);
            }
        }
    }

    /// End of stream: flush held bytes; an unterminated marker line is a cmd.
    pub fn finish(&mut self, out: &mut Vec<u8>, cmds: &mut Vec<String>) {
        if self.capturing {
            self.emit(cmds);
            self.capturing = false;
        }
        out.append(&mut self.pending);
    }

    /// Forwards the held bytes that can no longer begin a marker and starts
    /// capturing once one is complete.
    fn settle(&mut self, out: &mut Vec<u8>) {
        while !self.pending.is_empty() {
            match self.classify() {
                Prefix::Complete => {
                    self.pending.clear();
                    self.capturing = true;
                    return;
                }
                Prefix::Partial => return,
                Prefix::No => {
                    // A marker can only begin at a later `+`.
                    let next = self.pending[1..]
                        .iter()
                        .position(|&b| b == b'+')
                        .map_or(self.pending.len(), |i| i + 1);
                    out.extend(self.pending.drain(..next));
                }
            }
        }
    }

    fn classify(&self) -> Prefix {
        let plus = self.pending.iter().take_while(|&&b| b == b'+').count();
        if plus == 0 || plus > MAX_PLUS {
            return Prefix::No;
        }
        let rest = &self.pending[plus..];
        if rest.len() >= self.tail.len() {
            if rest.starts_with(&self.tail) {
                Prefix::Complete
            } else {
                Prefix::No
            }
        } else if self.tail.starts_with(rest) {
            Prefix::Partial
        } else {
            Prefix::No
        }
    }

    fn emit(&mut self, cmds: &mut Vec<String>) {
        let mut text = String::from_utf8_lossy(&self.cmd).trim_end().to_string();
        if text.len() > MAX_CMD_TEXT {
            let mut cut = MAX_CMD_TEXT;
            while !text.is_char_boundary(cut) {
                cut -= 1;
            }
            text.truncate(cut);
            self.truncated += 1;
        }
        cmds.push(text);
        self.cmd.clear();
    }
}

enum Prefix {
    No,
    Partial,
    Complete,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(token: &str, chunks: &[&[u8]]) -> (Vec<u8>, Vec<String>) {
        let mut f = MarkerFilter::new(token);
        let (mut out, mut cmds) = (Vec::new(), Vec::new());
        for c in chunks {
            f.feed(c, &mut out, &mut cmds);
        }
        f.finish(&mut out, &mut cmds);
        (out, cmds)
    }

    #[test]
    fn marker_lines_become_cmds_and_vanish() {
        let (out, cmds) = run("ab12", &[b"+sc-ab12> true\nreal output\n+sc-ab12> false\n"]);
        assert_eq!(out, b"real output\n");
        assert_eq!(cmds, ["true", "false"]);
    }

    #[test]
    fn marker_split_across_reads() {
        let (out, cmds) = run("ab12", &[b"+sc-a", b"b12> apt-get", b" install x\nok\n"]);
        assert_eq!(out, b"ok\n");
        assert_eq!(cmds, ["apt-get install x"]);
    }

    #[test]
    fn wrong_tokens_pass_through_at_line_start_and_mid_line() {
        let (out, cmds) = run("ab12", &[b"+sc-zz99> fake\nsay +sc-zz99> no\n"]);
        assert_eq!(out, b"+sc-zz99> fake\nsay +sc-zz99> no\n");
        assert!(cmds.is_empty());
    }

    #[test]
    fn marker_after_unterminated_output_is_a_cmd() {
        let (out, cmds) = run("ab12", &[b"x+sc-ab12> false\n"]);
        assert_eq!(out, b"x");
        assert_eq!(cmds, ["false"]);
    }

    #[test]
    fn mid_line_marker_split_across_reads() {
        let (out, cmds) = run("ab12", &[b"x+", b"+sc-ab", b"12> fal", b"se\nok\n"]);
        assert_eq!(out, b"xok\n");
        assert_eq!(cmds, ["false"]);
    }

    #[test]
    fn plus_signs_before_a_failed_prefix_are_forwarded() {
        let (out, cmds) = run("ab12", &[b"a+b++sc-ab+sc-ab12> c\n"]);
        assert_eq!(out, b"a+b++sc-ab");
        assert_eq!(cmds, ["c"]);
    }

    #[test]
    fn carriage_returns_pass_through() {
        let (out, _) = run("ab12", &[b"10%\r20%\r100%\n"]);
        assert_eq!(out, b"10%\r20%\r100%\n");
    }

    #[test]
    fn bash_nesting_plus_signs_are_accepted() {
        let (out, cmds) = run("ab12", &[b"+++sc-ab12> inner\n++ not a marker\n"]);
        assert_eq!(out, b"++ not a marker\n");
        assert_eq!(cmds, ["inner"]);
    }

    #[test]
    fn plus_run_longer_than_the_nesting_limit_keeps_only_the_limit() {
        let input = format!("{}sc-ab12> deep\n", "+".repeat(MAX_PLUS + 3));
        let (out, cmds) = run("ab12", &[input.as_bytes()]);
        assert_eq!(out, b"+++");
        assert_eq!(cmds, ["deep"]);
    }

    #[test]
    fn long_line_without_newline_is_forwarded_with_bounded_buffer() {
        let mut f = MarkerFilter::new("ab12");
        let (mut out, mut cmds) = (Vec::new(), Vec::new());
        let chunk = vec![b'x'; 1 << 20];
        f.feed(&chunk, &mut out, &mut cmds);
        assert_eq!(out.len(), chunk.len(), "nothing held back mid-line");
        f.feed(b"+sc-", &mut out, &mut cmds);
        assert_eq!(out.len(), chunk.len(), "only a possible marker is held");
        f.feed(b"x", &mut out, &mut cmds);
        assert_eq!(out.len(), chunk.len() + 5);
    }

    #[test]
    fn unterminated_marker_at_eof_is_a_cmd() {
        let (out, cmds) = run("ab12", &[b"+sc-ab12> last"]);
        assert!(out.is_empty());
        assert_eq!(cmds, ["last"]);
    }

    #[test]
    fn long_cmd_text_is_truncated_on_a_char_boundary() {
        let long = format!("+sc-ab12> {}\n", "é".repeat(2000));
        let mut f = MarkerFilter::new("ab12");
        let (mut out, mut cmds) = (Vec::new(), Vec::new());
        f.feed(long.as_bytes(), &mut out, &mut cmds);
        assert!(cmds[0].len() <= sandcastle_proto::MAX_CMD_TEXT);
        assert!(cmds[0].chars().all(|c| c == 'é'));
        assert_eq!(f.truncated(), 1);
    }
}
