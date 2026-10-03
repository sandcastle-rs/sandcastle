//! Separates `sh -x` trace lines from a RUN step's stderr. The shell runs
//! with `PS4` set to a per-job marker; lines that start with it are the
//! commands the shell is about to run. Everything else is forwarded as it
//! arrives, holding back at most a possible marker prefix.

use sandcastle_proto::MAX_CMD_TEXT;

/// Longest run of leading `+` accepted (bash repeats PS4's first character
/// once per nesting level).
const MAX_PLUS: usize = 64;

pub struct MarkerFilter {
    /// The marker after the leading `+` run: `sc-<token>> `.
    tail: Vec<u8>,
    state: State,
    /// Bytes held back while a line start may still be a marker.
    pending: Vec<u8>,
    /// Command text of the marker line being read.
    cmd: Vec<u8>,
    truncated: u64,
}

#[derive(Clone, Copy, PartialEq)]
enum State {
    /// At the start of a line, nothing held back.
    LineStart,
    /// `pending` is a possible marker prefix.
    Deciding,
    /// Inside an ordinary line.
    Passing,
    /// Inside a marker line; bytes go to `cmd`.
    Capturing,
}

impl MarkerFilter {
    pub fn new(token: &str) -> Self {
        Self {
            tail: format!("sc-{token}> ").into_bytes(),
            state: State::LineStart,
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
            match self.state {
                State::Passing => {
                    out.push(b);
                    if b == b'\n' {
                        self.state = State::LineStart;
                    }
                }
                State::Capturing => {
                    if b == b'\n' {
                        self.emit(cmds);
                        self.state = State::LineStart;
                    } else if self.cmd.len() <= MAX_CMD_TEXT * 4 {
                        self.cmd.push(b);
                    }
                }
                State::LineStart | State::Deciding => {
                    self.pending.push(b);
                    match self.classify() {
                        Prefix::Complete => {
                            self.pending.clear();
                            self.state = State::Capturing;
                        }
                        Prefix::Partial => self.state = State::Deciding,
                        Prefix::No => {
                            out.append(&mut self.pending);
                            self.state = if b == b'\n' {
                                State::LineStart
                            } else {
                                State::Passing
                            };
                        }
                    }
                }
            }
        }
    }

    /// End of stream: flush held bytes; an unterminated marker line is a cmd.
    pub fn finish(&mut self, out: &mut Vec<u8>, cmds: &mut Vec<String>) {
        match self.state {
            State::Capturing => self.emit(cmds),
            State::Deciding => out.append(&mut self.pending),
            State::LineStart | State::Passing => {}
        }
        self.state = State::LineStart;
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
    fn wrong_token_and_mid_line_markers_pass_through() {
        let (out, cmds) = run("ab12", &[b"+sc-zz99> fake\nsay +sc-ab12> no\n"]);
        assert_eq!(out, b"+sc-zz99> fake\nsay +sc-ab12> no\n");
        assert!(cmds.is_empty());
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
    fn long_line_without_newline_is_forwarded_with_bounded_buffer() {
        let mut f = MarkerFilter::new("ab12");
        let (mut out, mut cmds) = (Vec::new(), Vec::new());
        let chunk = vec![b'x'; 1 << 20];
        f.feed(&chunk, &mut out, &mut cmds);
        assert_eq!(out.len(), chunk.len(), "nothing held back mid-line");
        f.feed(b"+sc-", &mut out, &mut cmds);
        assert_eq!(
            out.len(),
            chunk.len() + 4,
            "mid-line text is never a marker"
        );
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
