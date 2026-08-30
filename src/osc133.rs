//! Passive scanner for OSC 133 "semantic prompt" marks in a PTY byte stream.
//!
//! Shells bracket their prompts and commands with `OSC 133 ; A/B/C/D[;args] ST` (FinalTerm's
//! convention: prompt start, command start, execution start, and command end with exit code).
//! fish emits these natively; zsh/bash need a small integration snippet (see the README).
//!
//! potty taps each pane's raw byte stream on its way to the terminal parser — the scanner
//! never consumes or alters bytes, it only watches — and reports finished commands' exit
//! codes, which drive the exit-status aura. `D` is only reported when a `C` (execution
//! start) preceded it, so shell startup and empty prompts don't count as commands.

/// Longest OSC payload we bother buffering. `133;…` marks are tiny; anything bigger (OSC 52
/// clipboard blobs, title strings) is skipped without storing.
const CAP: usize = 64;

#[derive(Default)]
enum State {
    #[default]
    Ground,
    /// Saw ESC; `]` opens an OSC.
    Esc,
    /// Inside an OSC, collecting the payload.
    Osc,
    /// Saw ESC inside an OSC; `\` (making ST) terminates it.
    OscEsc,
}

#[derive(Default)]
pub struct Scanner {
    state: State,
    buf: Vec<u8>,
    /// Payload outgrew `CAP` — keep tracking the sequence to its end, but don't parse it.
    overflow: bool,
    /// A `133;C` was seen since the last `133;D` — a command is actually running.
    executing: bool,
}

impl Scanner {
    /// Watch a chunk of PTY output (sequences may span chunks). Returns the exit codes of
    /// any commands that finished in this chunk, oldest first.
    pub fn scan(&mut self, bytes: &[u8]) -> Vec<i32> {
        let mut done = Vec::new();
        for &b in bytes {
            match self.state {
                State::Ground => {
                    if b == 0x1b {
                        self.state = State::Esc;
                    }
                }
                State::Esc => {
                    self.state = match b {
                        b']' => {
                            self.buf.clear();
                            self.overflow = false;
                            State::Osc
                        }
                        0x1b => State::Esc,
                        _ => State::Ground,
                    };
                }
                State::Osc => match b {
                    0x07 => {
                        self.finish(&mut done);
                        self.state = State::Ground;
                    }
                    0x1b => self.state = State::OscEsc,
                    _ => {
                        if self.buf.len() < CAP {
                            self.buf.push(b);
                        } else {
                            self.overflow = true;
                        }
                    }
                },
                State::OscEsc => {
                    if b == b'\\' {
                        self.finish(&mut done);
                        self.state = State::Ground;
                    } else {
                        // Not an ST — the OSC was aborted mid-sequence. The ESC we swallowed
                        // starts something new; re-dispatch this byte as if following it.
                        self.state = match b {
                            b']' => {
                                self.buf.clear();
                                self.overflow = false;
                                State::Osc
                            }
                            0x1b => State::Esc,
                            _ => State::Ground,
                        };
                    }
                }
            }
        }
        done
    }

    /// A complete OSC payload is in `buf` — react if it's a 133 mark.
    fn finish(&mut self, done: &mut Vec<i32>) {
        if self.overflow {
            return;
        }
        match self.buf.strip_prefix(b"133;") {
            Some([b'C', ..]) => self.executing = true,
            // A new prompt implies the previous command is over — self-heals a lost `D`.
            Some([b'A', ..]) => self.executing = false,
            Some([b'D', rest @ ..]) if self.executing => {
                self.executing = false;
                done.push(parse_code(rest));
            }
            _ => {}
        }
    }
}

/// The exit code after `133;D` — empty or malformed reads as 0. Only the first
/// `;`-separated field counts: kitty-style integrations append parameters
/// (`133;D;1;aid=…`) that must not spoil the number.
fn parse_code(rest: &[u8]) -> i32 {
    match rest.strip_prefix(b";") {
        Some(args) => {
            let first = args.split(|&b| b == b';').next().unwrap_or(&[]);
            std::str::from_utf8(first)
                .ok()
                .and_then(|s| s.trim().parse().ok())
                .unwrap_or(0)
        }
        None => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_exit_code_after_exec() {
        let mut s = Scanner::default();
        assert_eq!(
            s.scan(b"\x1b]133;C\x07make: error\r\n\x1b]133;D;2\x07"),
            [2]
        );
    }

    #[test]
    fn st_terminator_works() {
        let mut s = Scanner::default();
        assert_eq!(s.scan(b"\x1b]133;C\x1b\\\x1b]133;D;1\x1b\\"), [1]);
    }

    #[test]
    fn d_without_preceding_c_is_ignored() {
        // Shell startup: the first precmd fires D before any command ran.
        let mut s = Scanner::default();
        assert!(s.scan(b"\x1b]133;D;0\x07\x1b]133;A\x07").is_empty());
    }

    #[test]
    fn kitty_style_marks_with_extra_params() {
        // kitty's integration decorates its marks (`A;cl=m;aid=…`, `C;`, `D;1;aid=…`) —
        // the code is the first field, the rest must be ignored.
        let mut s = Scanner::default();
        assert_eq!(
            s.scan(b"\x1b]133;A;cl=m;aid=42\x07\x1b]133;C;\x07\x1b]133;D;1;aid=42\x07"),
            [1]
        );
    }

    #[test]
    fn d_without_code_reads_as_zero() {
        let mut s = Scanner::default();
        assert_eq!(s.scan(b"\x1b]133;C\x07\x1b]133;D\x07"), [0]);
    }

    #[test]
    fn sequences_split_across_chunks() {
        let mut s = Scanner::default();
        let stream = b"\x1b]133;C\x07out\x1b]133;D;130\x07";
        let mut got = Vec::new();
        for chunk in stream.chunks(3) {
            got.extend(s.scan(chunk));
        }
        assert_eq!(got, [130]);
    }

    #[test]
    fn multiple_commands_in_one_chunk() {
        let mut s = Scanner::default();
        assert_eq!(
            s.scan(b"\x1b]133;C\x07\x1b]133;D;1\x07\x1b]133;C\x07\x1b]133;D;0\x07"),
            [1, 0]
        );
    }

    #[test]
    fn other_oscs_are_ignored() {
        let mut s = Scanner::default();
        assert_eq!(
            s.scan(b"\x1b]2;title\x07\x1b]133;C\x07\x1b]133;D;7\x07"),
            [7]
        );
    }

    #[test]
    fn oversized_osc_is_skipped_without_confusion() {
        let mut s = Scanner::default();
        let mut stream = b"\x1b]52;c;".to_vec();
        stream.extend(std::iter::repeat_n(b'Q', 4096));
        stream.extend(b"\x07\x1b]133;C\x07\x1b]133;D;3\x07");
        assert_eq!(s.scan(&stream), [3]);
    }

    #[test]
    fn prompt_mark_resets_a_lost_command() {
        // C … (D lost) … A, then a D from the next precmd must not fire.
        let mut s = Scanner::default();
        assert!(
            s.scan(b"\x1b]133;C\x07\x1b]133;A\x07\x1b]133;D;0\x07")
                .is_empty()
        );
    }

    #[test]
    fn aborted_osc_does_not_eat_following_sequences() {
        // ESC inside the OSC followed by something that isn't ST aborts it; the next
        // sequence must still parse.
        let mut s = Scanner::default();
        assert_eq!(s.scan(b"\x1b]0;tit\x1b]133;C\x07\x1b]133;D;9\x07"), [9]);
    }
}
