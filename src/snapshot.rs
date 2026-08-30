//! Serialize a terminal's state back into an escape-sequence stream.
//!
//! `potty-session` keeps a headless `alacritty_terminal::Term` per pane and, on (re)attach, sends
//! the client a snapshot of that grid instead of replaying raw PTY output. Replaying raw bytes can
//! never be safe: once the buffer is trimmed, stateful sequences lose their opening half — a
//! truncated alt-screen episode (helix, vim) replays its absolute-position paints onto the primary
//! screen and the junk ends up in scrollback. A snapshot sidesteps the whole class: alt-screen
//! frames never enter the primary grid or history, so they can't leak into what we send.
//!
//! The stream a snapshot produces is plain VT/xterm output: styled text for history + screen
//! (run-length SGR, wrap-aware), a cursor position, the active private modes, and the title. The
//! client needs no special handling — it parses the snapshot like any other pane output.
//!
//! Not restored (all self-heal on the app's next repaint, which the post-attach resize triggers):
//! scroll regions, charsets, saved cursors, tab stops, cursor shape, pending-wrap state.

use std::fmt::Write;

use alacritty_terminal::event::EventListener;
use alacritty_terminal::grid::{Dimensions, Grid};
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::term::{Term, TermMode};
use alacritty_terminal::vte::ansi::{Color, NamedColor};

/// Serialize `term` (history, screen, cursor, modes, title) into an escape stream that recreates
/// it when fed to a freshly created terminal of the same size.
///
/// Takes `&mut` only for the alt-screen case: the inactive (primary) grid is private, so we swap
/// it in to read it and swap back. The terminal's observable state is unchanged afterwards.
pub fn serialize<T: EventListener>(term: &mut Term<T>, title: Option<&str>) -> Vec<u8> {
    let mut out = String::new();
    let mode = *term.mode();
    let mut sgr = SgrState::default();

    if mode.contains(TermMode::ALT_SCREEN) {
        // Serialize the primary screen first so leaving the alt screen later lands the client on
        // the right content. The primary grid is `Term`'s private `inactive_grid`; `swap_alt` is
        // the only public way in. Swapping back re-enters the alt screen, which *resets* the
        // inactive grid — so clone the alt grid up front and restore it (contents + cursor)
        // afterwards. `swap_alt` also clobbers the primary's saved cursor; acceptable, it only
        // matters for a DECRC the app would follow with a repaint anyway.
        let alt = term.grid().clone();
        term.swap_alt();
        write_grid(&mut out, term.grid(), &mut sgr);
        out.push_str("\x1b[0m");
        sgr = SgrState::default();
        cup(&mut out, term.grid().cursor.point);
        term.swap_alt();
        *term.grid_mut() = alt;

        out.push_str("\x1b[?1049h");
        write_grid(&mut out, term.grid(), &mut sgr);
        out.push_str("\x1b[0m");
        cup(&mut out, term.grid().cursor.point);
    } else {
        write_grid(&mut out, term.grid(), &mut sgr);
        out.push_str("\x1b[0m");
        cup(&mut out, term.grid().cursor.point);
    }

    write_modes(&mut out, mode);
    if let Some(title) = title {
        let _ = write!(out, "\x1b]2;{title}\x07");
    }
    out.into_bytes()
}

/// History plus visible screen, top to bottom, as styled text. Rows are joined with `\r\n` except
/// after a soft-wrapped row, where emitting all `cols` cells leaves the receiver in pending-wrap
/// so the next row continues it and the WRAPLINE flag is recreated. Trailing cells that would be
/// invisible are dropped; blank rows still get their separator so the screen alignment (and the
/// history/screen boundary) is preserved exactly.
fn write_grid(out: &mut String, grid: &Grid<Cell>, sgr: &mut SgrState) {
    let cols = grid.columns();
    let top = grid.topmost_line().0;
    let bottom = grid.bottommost_line().0;
    let mut prev_wrapped = false;
    for l in top..=bottom {
        if l != top && !prev_wrapped {
            out.push_str("\r\n");
        }
        let row = &grid[Line(l)];
        let wrapped = row[Column(cols - 1)].flags.contains(Flags::WRAPLINE);
        let end = if wrapped {
            cols
        } else {
            (0..cols)
                .rev()
                .find(|&c| has_content(&row[Column(c)]))
                .map_or(0, |c| c + 1)
        };
        for c in 0..end {
            let cell = &row[Column(c)];
            // Spacers are artifacts of wide-char layout; re-printing the wide char recreates
            // them (including the leading spacer + wrap when one didn't fit at end of line).
            if cell
                .flags
                .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
            {
                continue;
            }
            sgr.emit(out, cell);
            out.push(cell.c);
            if let Some(zerowidth) = cell.zerowidth() {
                out.extend(zerowidth);
            }
        }
        prev_wrapped = wrapped;
    }
}

/// A cell the receiver's default-blank cell wouldn't render identically to.
fn has_content(cell: &Cell) -> bool {
    cell.c != ' '
        || cell.bg != Color::Named(NamedColor::Background)
        || cell
            .flags
            .intersects(Flags::INVERSE | Flags::ALL_UNDERLINES | Flags::STRIKEOUT)
        || cell.zerowidth().is_some()
}

fn cup(out: &mut String, point: Point) {
    let _ = write!(out, "\x1b[{};{}H", point.line.0 + 1, point.column.0 + 1);
}

/// Private modes the pane had switched on (or off, where the default is on). DECSET state must
/// survive reattach — a mouse-mode app on the alt screen expects clicks reported, a shell expects
/// bracketed paste — and the client's fresh terminal starts from defaults.
fn write_modes(out: &mut String, mode: TermMode) {
    if !mode.contains(TermMode::SHOW_CURSOR) {
        out.push_str("\x1b[?25l");
    }
    if mode.contains(TermMode::APP_CURSOR) {
        out.push_str("\x1b[?1h");
    }
    if mode.contains(TermMode::APP_KEYPAD) {
        out.push_str("\x1b=");
    }
    if !mode.contains(TermMode::LINE_WRAP) {
        out.push_str("\x1b[?7l");
    }
    if mode.contains(TermMode::LINE_FEED_NEW_LINE) {
        out.push_str("\x1b[20h");
    }
    if mode.contains(TermMode::INSERT) {
        out.push_str("\x1b[4h");
    }
    for (bit, n) in [
        (TermMode::MOUSE_REPORT_CLICK, 1000),
        (TermMode::MOUSE_DRAG, 1002),
        (TermMode::MOUSE_MOTION, 1003),
        (TermMode::FOCUS_IN_OUT, 1004),
        (TermMode::UTF8_MOUSE, 1005),
        (TermMode::SGR_MOUSE, 1006),
        (TermMode::ALTERNATE_SCROLL, 1007),
        (TermMode::BRACKETED_PASTE, 2004),
    ] {
        if mode.contains(bit) {
            let _ = write!(out, "\x1b[?{n}h");
        }
    }
    let kitty: u8 = [
        (TermMode::DISAMBIGUATE_ESC_CODES, 1),
        (TermMode::REPORT_EVENT_TYPES, 2),
        (TermMode::REPORT_ALTERNATE_KEYS, 4),
        (TermMode::REPORT_ALL_KEYS_AS_ESC, 8),
        (TermMode::REPORT_ASSOCIATED_TEXT, 16),
    ]
    .iter()
    .filter(|(bit, _)| mode.contains(*bit))
    .map(|(_, flag)| flag)
    .sum();
    if kitty != 0 {
        let _ = write!(out, "\x1b[={kitty};1u");
    }
}

/// Run-length SGR emitter: one full restated attribute set whenever a cell's style differs from
/// the previous cell's, nothing in between.
struct SgrState {
    fg: Color,
    bg: Color,
    flags: Flags,
    underline: Option<Color>,
}

impl Default for SgrState {
    fn default() -> Self {
        Self {
            fg: Color::Named(NamedColor::Foreground),
            bg: Color::Named(NamedColor::Background),
            flags: Flags::empty(),
            underline: None,
        }
    }
}

impl SgrState {
    fn emit(&mut self, out: &mut String, cell: &Cell) {
        let style = cell.flags
            & (Flags::INVERSE
                | Flags::BOLD
                | Flags::ITALIC
                | Flags::DIM
                | Flags::HIDDEN
                | Flags::STRIKEOUT
                | Flags::ALL_UNDERLINES);
        let underline = cell.underline_color();
        if self.fg == cell.fg
            && self.bg == cell.bg
            && self.flags == style
            && self.underline == underline
        {
            return;
        }

        out.push_str("\x1b[0");
        if style.contains(Flags::BOLD) {
            out.push_str(";1");
        }
        if style.contains(Flags::DIM) {
            out.push_str(";2");
        }
        if style.contains(Flags::ITALIC) {
            out.push_str(";3");
        }
        if style.contains(Flags::UNDERLINE) {
            out.push_str(";4");
        } else if style.contains(Flags::DOUBLE_UNDERLINE) {
            out.push_str(";4:2");
        } else if style.contains(Flags::UNDERCURL) {
            out.push_str(";4:3");
        } else if style.contains(Flags::DOTTED_UNDERLINE) {
            out.push_str(";4:4");
        } else if style.contains(Flags::DASHED_UNDERLINE) {
            out.push_str(";4:5");
        }
        if style.contains(Flags::INVERSE) {
            out.push_str(";7");
        }
        if style.contains(Flags::HIDDEN) {
            out.push_str(";8");
        }
        if style.contains(Flags::STRIKEOUT) {
            out.push_str(";9");
        }
        push_color(out, cell.fg, 30, 38);
        push_color(out, cell.bg, 40, 48);
        match underline {
            Some(Color::Indexed(i)) => {
                let _ = write!(out, ";58;5;{i}");
            }
            Some(Color::Spec(rgb)) => {
                let _ = write!(out, ";58;2;{};{};{}", rgb.r, rgb.g, rgb.b);
            }
            _ => {}
        }
        out.push('m');

        self.fg = cell.fg;
        self.bg = cell.bg;
        self.flags = style;
        self.underline = underline;
    }
}

fn push_color(out: &mut String, color: Color, named_base: usize, extended: usize) {
    match color {
        Color::Named(named) => {
            // 0..=7 standard, 8..=15 bright; Foreground/Background are the reset defaults the
            // leading `0` already restored, and the Dim/Cursor variants never land in cells.
            let idx = named as usize;
            if idx < 8 {
                let _ = write!(out, ";{}", named_base + idx);
            } else if idx < 16 {
                let _ = write!(out, ";{}", named_base + 60 + idx - 8);
            }
        }
        Color::Indexed(i) => {
            let _ = write!(out, ";{extended};5;{i}");
        }
        Color::Spec(rgb) => {
            let _ = write!(out, ";{extended};2;{};{};{}", rgb.r, rgb.g, rgb.b);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alacritty_terminal::event::VoidListener;
    use alacritty_terminal::term::Config;
    use alacritty_terminal::term::test::TermSize;
    use alacritty_terminal::vte::ansi::{Processor, StdSyncHandler};

    const COLS: usize = 20;
    const ROWS: usize = 5;
    const HISTORY: usize = 100;

    struct Harness {
        term: Term<VoidListener>,
        parser: Processor<StdSyncHandler>,
    }

    impl Harness {
        fn new() -> Self {
            let config = Config {
                scrolling_history: HISTORY,
                ..Config::default()
            };
            Self {
                term: Term::new(config, &TermSize::new(COLS, ROWS), VoidListener),
                parser: Processor::new(),
            }
        }

        fn feed(&mut self, bytes: &[u8]) {
            self.parser.advance(&mut self.term, bytes);
        }

        fn snapshot(&mut self) -> Vec<u8> {
            serialize(&mut self.term, None)
        }
    }

    fn round_trip(original: &mut Harness) -> Harness {
        let bytes = original.snapshot();
        let mut restored = Harness::new();
        restored.feed(&bytes);
        restored
    }

    /// The two grids render identically: same history depth, and every cell either matches or
    /// both are visually blank (a trimmed trailing cell only keeps its bg/decoration guarantee).
    fn assert_same_screen(a: &Term<VoidListener>, b: &Term<VoidListener>) {
        assert_eq!(
            a.grid().history_size(),
            b.grid().history_size(),
            "history depth"
        );
        assert_eq!(a.grid().cursor.point, b.grid().cursor.point, "cursor");
        let top = a.grid().topmost_line().0;
        for l in top..ROWS as i32 {
            for c in 0..COLS {
                let ca = &a.grid()[Line(l)][Column(c)];
                let cb = &b.grid()[Line(l)][Column(c)];
                let visible = |cell: &Cell| has_content(cell) || cell.c != ' ';
                if !visible(ca) && !visible(cb) {
                    continue;
                }
                assert_eq!(ca.c, cb.c, "char at line {l} col {c}");
                assert_eq!(ca.fg, cb.fg, "fg at line {l} col {c}");
                assert_eq!(ca.bg, cb.bg, "bg at line {l} col {c}");
                let mask = Flags::INVERSE
                    | Flags::BOLD
                    | Flags::ITALIC
                    | Flags::DIM
                    | Flags::HIDDEN
                    | Flags::STRIKEOUT
                    | Flags::ALL_UNDERLINES
                    | Flags::WRAPLINE
                    | Flags::WIDE_CHAR
                    | Flags::WIDE_CHAR_SPACER
                    | Flags::LEADING_WIDE_CHAR_SPACER;
                assert_eq!(
                    ca.flags & mask,
                    cb.flags & mask,
                    "flags at line {l} col {c}"
                );
            }
        }
    }

    fn screen_text(term: &Term<VoidListener>) -> String {
        let mut text = String::new();
        let top = term.grid().topmost_line().0;
        for l in top..ROWS as i32 {
            for c in 0..COLS {
                text.push(term.grid()[Line(l)][Column(c)].c);
            }
        }
        text
    }

    #[test]
    fn plain_text_round_trips() {
        let mut orig = Harness::new();
        orig.feed(b"$ echo hi\r\nhi\r\n$ ");
        let restored = round_trip(&mut orig);
        assert_same_screen(&orig.term, &restored.term);
        assert!(screen_text(&restored.term).contains("echo hi"));
    }

    #[test]
    fn styles_round_trip() {
        let mut orig = Harness::new();
        orig.feed(b"\x1b[1;31mred\x1b[0m \x1b[3;48;5;27midx\x1b[0m\r\n");
        orig.feed(b"\x1b[4;38;2;10;200;30mtrue\x1b[0m \x1b[7minv\x1b[0m");
        let restored = round_trip(&mut orig);
        assert_same_screen(&orig.term, &restored.term);
    }

    #[test]
    fn history_round_trips() {
        let mut orig = Harness::new();
        for i in 0..40 {
            orig.feed(format!("line {i}\r\n").as_bytes());
        }
        let restored = round_trip(&mut orig);
        assert_eq!(orig.term.grid().history_size(), 40 - ROWS + 1);
        assert_same_screen(&orig.term, &restored.term);
        assert!(screen_text(&restored.term).contains("line 0"));
    }

    #[test]
    fn wide_chars_and_wrapping_round_trip() {
        let mut orig = Harness::new();
        orig.feed("日本語 wide\r\n".as_bytes());
        orig.feed(b"a long line that soft-wraps across rows without a newline");
        let restored = round_trip(&mut orig);
        assert_same_screen(&orig.term, &restored.term);
    }

    #[test]
    fn cursor_and_modes_round_trip() {
        let mut orig = Harness::new();
        orig.feed(b"prompt\x1b[?1h\x1b[?2004h\x1b[?1002h\x1b[?1006h\x1b[?25l\x1b[3;7H");
        let restored = round_trip(&mut orig);
        assert_same_screen(&orig.term, &restored.term);
        for bit in [
            TermMode::APP_CURSOR,
            TermMode::BRACKETED_PASTE,
            TermMode::MOUSE_DRAG,
            TermMode::SGR_MOUSE,
        ] {
            assert!(restored.term.mode().contains(bit), "{bit:?} not restored");
        }
        assert!(!restored.term.mode().contains(TermMode::SHOW_CURSOR));
    }

    /// The bug that motivated snapshots: a finished full-screen-app episode (helix, vim) must
    /// leave zero trace, no matter how much it painted. With raw replay, trimming used to cut the
    /// alt-screen entry off and the paints landed in the primary screen and scrollback.
    #[test]
    fn finished_alt_screen_episode_leaves_no_trace() {
        let mut orig = Harness::new();
        orig.feed(b"$ hx file\r\n");
        orig.feed(b"\x1b[?1049h");
        for frame in 0..2000 {
            for row in 1..=ROWS {
                orig.feed(format!("\x1b[{row};1H\x1b[38;2;1;2;3mXJUNKX {frame}").as_bytes());
            }
        }
        orig.feed(b"\x1b[?1049l$ ");
        let bytes = orig.snapshot();
        let junk = b"XJUNKX";
        assert!(
            !bytes.windows(junk.len()).any(|w| w == junk),
            "alt-screen paints leaked into the snapshot"
        );
        let mut restored = Harness::new();
        restored.feed(&bytes);
        assert_same_screen(&orig.term, &restored.term);
        assert!(!screen_text(&restored.term).contains("XJUNKX"));
    }

    /// Reattaching while the app is still on the alt screen: the snapshot recreates the primary
    /// screen underneath, then the alt screen on top — and serializing must not disturb the
    /// running terminal (the swap-and-restore dance is invisible).
    #[test]
    fn active_alt_screen_round_trips() {
        let mut orig = Harness::new();
        orig.feed(b"$ hx file\r\n");
        orig.feed(b"\x1b[?1049h\x1b[2;3H\x1b[1mEDITOR\x1b[0m");

        let first = orig.snapshot();
        let second = orig.snapshot();
        assert_eq!(first, second, "serializing twice must not perturb the term");

        let mut restored = Harness::new();
        restored.feed(&first);
        assert!(restored.term.mode().contains(TermMode::ALT_SCREEN));
        assert_same_screen(&orig.term, &restored.term);
        assert!(screen_text(&restored.term).contains("EDITOR"));

        // Leave the alt screen on both: the primary screens must match too.
        orig.feed(b"\x1b[?1049l");
        restored.feed(b"\x1b[?1049l");
        assert_same_screen(&orig.term, &restored.term);
        assert!(screen_text(&restored.term).contains("hx file"));
    }

    #[test]
    fn title_is_included() {
        let mut orig = Harness::new();
        orig.feed(b"$ ");
        let bytes = serialize(&mut orig.term, Some("build box"));
        let osc = b"\x1b]2;build box\x07";
        assert!(bytes.windows(osc.len()).any(|w| w == osc));
    }
}
