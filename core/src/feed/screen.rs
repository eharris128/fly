//! Screen-derived pending-interaction parsing
//! (feed-question-screen-fallback U2, KTD2/KTD3, R3/R4).
//!
//! Claude Code v2.1.206 no longer flushes a pending interaction's `tool_use`
//! to the transcript at ask time, so while a dialog is open its body exists in
//! exactly two places: Claude's process memory, and the PTY byte stream fly
//! owns. This module turns a pane's raw output tail (`pty::ScreenTail`) into a
//! parsed picker — or, far more often than it guesses, **nothing**.
//!
//! Two layers, both pure and fixture-tested:
//! - [`render_tail`]: play the bytes through a minimal VT interpreter (`vte`
//!   tokenizes; the grid semantics here are deliberately small). Starting from
//!   a blank grid mid-stream is sound *for this use* because Claude Code's Ink
//!   UI repaints the whole dialog per frame anchored at `ESC[H`, and the
//!   matcher works on content patterns, not absolute coordinates (KTD1 of the
//!   plan). Any sequence whose effect we can't reproduce sets a `surprised`
//!   taint that forces abstention (R3).
//! - [`parse_interaction`]: shape-strict matching of the rendered picker —
//!   one numbered block `[❯] N. label`, digits exactly `1..=N` in order,
//!   exactly one cursor, a question line above, an `Esc`-bearing footer below.
//!   Digits are exposed **as rendered** (R4): they are, by construction, the
//!   keys a `mode:"keys"` answer will deliver.
//!
//! Every string leaving this module is raw and untrusted (rendered agent
//! output); the caller runs the R8/KTD7 `clean` pipeline before exposure.

use vte::{Params, Parser, Perform};

/// Cap on grid rows retained while replaying the tail: older rows scroll off.
/// The dialog is the last thing painted, so the tail rows are what matter; the
/// cap bounds memory against a garbage flood.
const MAX_GRID_ROWS: usize = 200;

/// A parsed on-screen interaction (KTD3). All strings raw/untrusted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScreenInteraction {
    pub kind: ScreenKind,
    /// The question line(s) above the option block, joined by single spaces.
    pub question: String,
    /// The header chip above the question (e.g. `Color preference`), when one
    /// was recognizable; empty otherwise.
    pub header: String,
    /// Permission dialogs: the box body above the "Do you want …" line — the
    /// dialog title (first line, e.g. `Bash command`) followed by the request
    /// lines. Empty for a choice picker.
    pub context: Vec<String>,
    /// The rendered options, digits as displayed (R4).
    pub options: Vec<ScreenOption>,
    /// Index into `options` of the `❯`-cursored row.
    pub cursor_at: usize,
}

/// Which dialog shape the screen shows (mirrors `transcript::PendingKind`,
/// classified from the rendered text alone — never from the attention reason,
/// which v2.1.206 blurs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScreenKind {
    Choice,
    Permission,
}

/// One rendered option: the digit as displayed, the label on the numbered
/// line, and any indented continuation lines folded in as the description.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScreenOption {
    pub digit: u32,
    pub label: String,
    pub description: String,
}

/// Parse a pane's raw output tail into a pending interaction, or `None` —
/// which the caller treats as "pending, body unavailable" (R2 two-tier
/// degrade), never as evidence nothing is pending.
pub fn parse_screen_interaction(bytes: &[u8], cols: u16) -> Option<ScreenInteraction> {
    let grid = render_tail(bytes, cols);
    if grid.surprised {
        return None;
    }
    parse_interaction(&grid.text_lines())
}

// ---- the minimal VT grid (KTD2) ---------------------------------------------

/// Line-oriented screen state built from a byte tail. `surprised` means a
/// sequence outside the supported set was seen whose effect could corrupt the
/// layout — the caller must abstain (R3).
struct Grid {
    lines: Vec<Vec<char>>,
    row: usize,
    col: usize,
    cols: usize,
    saved: Option<(usize, usize)>,
    surprised: bool,
}

/// Replay `bytes` into a fresh grid rendered at `cols` columns.
fn render_tail(bytes: &[u8], cols: u16) -> Grid {
    let mut grid = Grid::new(cols as usize);
    let mut parser = Parser::new();
    for &b in bytes {
        parser.advance(&mut grid, b);
    }
    grid
}

impl Grid {
    fn new(cols: usize) -> Self {
        Self {
            lines: vec![Vec::new()],
            row: 0,
            col: 0,
            cols: cols.max(2),
            saved: None,
            surprised: false,
        }
    }

    /// The rendered text, one string per row, trailing blanks trimmed.
    fn text_lines(&self) -> Vec<String> {
        self.lines
            .iter()
            .map(|l| {
                let s: String = l.iter().collect();
                s.trim_end().to_string()
            })
            .collect()
    }

    fn ensure_row(&mut self) {
        // Clamp before allocating (audit-remediation U8/KTD8): a CSI cursor
        // jump can put `row` up to 65535 past the grid, and pushing every
        // intermediate row before the drain below would transiently allocate
        // ~65k Vecs. When the jump exceeds the retained window, every
        // surviving row would be a fresh empty anyway — so drop the old rows
        // now and land the cursor at the cap. Final state is identical to the
        // push-then-drain result; peak allocation is O(MAX_GRID_ROWS) always.
        if self.row >= self.lines.len() + MAX_GRID_ROWS {
            self.lines.clear();
            self.row = MAX_GRID_ROWS - 1;
        }
        while self.lines.len() <= self.row {
            self.lines.push(Vec::new());
        }
        // Row cap: scroll the oldest rows off, keeping the tail (bounded
        // memory; the dialog is the newest content).
        if self.lines.len() > MAX_GRID_ROWS {
            let drop = self.lines.len() - MAX_GRID_ROWS;
            self.lines.drain(..drop);
            self.row = self.row.saturating_sub(drop);
        }
    }

    fn put(&mut self, c: char) {
        if self.col >= self.cols {
            self.row += 1;
            self.col = 0;
        }
        self.ensure_row();
        let line = &mut self.lines[self.row];
        while line.len() <= self.col {
            line.push(' ');
        }
        line[self.col] = c;
        self.col += 1;
    }

    fn clear_line_from(&mut self, col: usize) {
        self.ensure_row();
        self.lines[self.row].truncate(col);
    }

    fn clear_line_to(&mut self, col: usize) {
        self.ensure_row();
        let line = &mut self.lines[self.row];
        for i in 0..=col.min(line.len().saturating_sub(1)) {
            line[i] = ' ';
        }
    }

    fn clear_below(&mut self) {
        self.clear_line_from(self.col);
        self.lines.truncate(self.row + 1);
    }

    fn clear_above(&mut self) {
        for i in 0..self.row {
            self.lines[i].clear();
        }
        self.clear_line_to(self.col);
    }

    fn clear_all(&mut self) {
        self.lines = vec![Vec::new()];
        self.row = 0;
        // Column deliberately kept: ED(2) does not move the cursor.
        self.ensure_row();
    }
}

/// First subparameter of the nth CSI param, with a default.
fn param(params: &Params, idx: usize, default: u16) -> u16 {
    params
        .iter()
        .nth(idx)
        .and_then(|p| p.first().copied())
        .filter(|&v| v != 0)
        .unwrap_or(default)
}

impl Perform for Grid {
    fn print(&mut self, c: char) {
        self.put(c);
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            b'\n' | 0x0b | 0x0c => self.row += 1, // LF/VT/FF: index
            b'\r' => self.col = 0,
            0x08 => self.col = self.col.saturating_sub(1), // BS
            b'\t' => self.col = (self.col / 8 + 1) * 8,
            0x07 => {} // BEL
            _ => {}    // other C0s are cosmetic for a line grid
        }
        self.ensure_row();
    }

    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], _ignore: bool, action: char) {
        // Private-mode sets/resets (`ESC[?…h/l`) and anything with a `>`/`?`
        // intermediate (queries, XTVERSION) don't move the layout.
        if !intermediates.is_empty() {
            return;
        }
        match action {
            'A' => self.row = self.row.saturating_sub(param(params, 0, 1) as usize),
            'B' | 'e' => self.row += param(params, 0, 1) as usize,
            'C' | 'a' => self.col += param(params, 0, 1) as usize,
            'D' => self.col = self.col.saturating_sub(param(params, 0, 1) as usize),
            'E' => {
                self.row += param(params, 0, 1) as usize;
                self.col = 0;
            }
            'F' => {
                self.row = self.row.saturating_sub(param(params, 0, 1) as usize);
                self.col = 0;
            }
            'G' | '`' => self.col = param(params, 0, 1) as usize - 1,
            'd' => self.row = param(params, 0, 1) as usize - 1,
            'H' | 'f' => {
                self.row = param(params, 0, 1) as usize - 1;
                self.col = param(params, 1, 1) as usize - 1;
            }
            'J' => {
                let mode = params.iter().next().and_then(|p| p.first().copied()).unwrap_or(0);
                match mode {
                    0 => self.clear_below(),
                    1 => self.clear_above(),
                    2 | 3 => self.clear_all(),
                    _ => self.surprised = true,
                }
            }
            'K' => {
                let mode = params.iter().next().and_then(|p| p.first().copied()).unwrap_or(0);
                match mode {
                    0 => self.clear_line_from(self.col),
                    1 => self.clear_line_to(self.col),
                    2 => self.clear_line_from(0),
                    _ => self.surprised = true,
                }
            }
            'm' => {} // SGR: colors/attrs are invisible to a text grid
            'h' | 'l' => {} // (non-private handled here too: insert mode etc. unused by Ink)
            'r' => {
                // DECSTBM: a full reset (`ESC[r`, no params) is Ink's startup
                // hygiene — harmless. Note vte reports a bare `ESC[r` as one
                // defaulted 0-param, so "has params" is NOT the reset test
                // (that mistake tainted every capture that included spawn
                // bytes — fix-feed-question-detection-gaps, pinned by the
                // ask-declined-reask-80 fixture). An actual sub-region
                // (nonzero top/bottom) changes scroll semantics we don't
                // model → surprise.
                if params.iter().any(|p| p.first().copied().unwrap_or(0) != 0) {
                    self.surprised = true;
                }
            }
            's' => self.saved = Some((self.row, self.col)),
            'u' => {
                if let Some((r, c)) = self.saved {
                    self.row = r;
                    self.col = c;
                }
            }
            't' | 'c' | 'n' | 'q' => {} // window ops / DA / DSR / cursor style: no layout effect
            _ => self.surprised = true,
        }
        self.ensure_row();
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], _ignore: bool, byte: u8) {
        if !intermediates.is_empty() {
            return; // charset designation etc. — no layout effect
        }
        match byte {
            b'7' => self.saved = Some((self.row, self.col)),
            b'8' => {
                if let Some((r, c)) = self.saved {
                    self.row = r;
                    self.col = c;
                }
            }
            b'D' => self.row += 1,        // IND
            b'E' => {
                self.row += 1;            // NEL
                self.col = 0;
            }
            b'M' => self.row = self.row.saturating_sub(1), // RI
            b'c' => self.clear_all(),     // RIS
            b'=' | b'>' | b'\\' => {}     // keypad modes / ST
            _ => self.surprised = true,
        }
        self.ensure_row();
    }

    // OSC (window title), DCS: no layout effect.
    fn osc_dispatch(&mut self, _params: &[&[u8]], _bell_terminated: bool) {}
    fn hook(&mut self, _params: &Params, _intermediates: &[u8], _ignore: bool, _action: char) {}
    fn put(&mut self, _byte: u8) {}
    fn unhook(&mut self) {}
}

// ---- the shape-strict matcher (KTD3) ----------------------------------------

/// The option cursor glyph Claude Code renders.
const CURSOR: char = '\u{276f}'; // ❯

/// A line parsed as `[❯] <digit>. <label>`.
struct OptionLine {
    row: usize,
    digit: u32,
    label: String,
    cursored: bool,
    /// Leading indent of the digit (used to tell continuation lines apart).
    indent: usize,
}

/// Try to read a rendered option line. Accepts an optional `❯` before the
/// number; requires `<digits>.` followed by at least one space or EOL.
fn option_line(row: usize, line: &str) -> Option<OptionLine> {
    let mut cursored = false;
    let mut rest = line;
    let mut indent = 0usize;
    // Leading spaces.
    while let Some(r) = rest.strip_prefix(' ') {
        rest = r;
        indent += 1;
    }
    if let Some(r) = rest.strip_prefix(CURSOR) {
        cursored = true;
        rest = r;
        indent += 1;
        while let Some(r) = rest.strip_prefix(' ') {
            rest = r;
            indent += 1;
        }
    }
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() || digits.len() > 2 {
        return None;
    }
    let after = &rest[digits.len()..];
    let label = after.strip_prefix('.')?;
    if !(label.is_empty() || label.starts_with(' ')) {
        return None; // `12.5` etc. — not an option row
    }
    Some(OptionLine {
        row,
        digit: digits.parse().ok()?,
        label: label.trim().to_string(),
        cursored,
        indent,
    })
}

/// A horizontal-rule line (the picker draws one before its trailing
/// "Chat about this" extra): nothing but box-drawing dashes and spaces.
fn is_rule(line: &str) -> bool {
    let t = line.trim();
    !t.is_empty() && t.chars().all(|c| matches!(c, '─' | '━' | '-' | '═'))
}

/// Whether a line looks like a picker footer — every observed dialog footer
/// carries an `Esc` hint (`Enter to select · … · Esc to cancel`,
/// `Esc to cancel · Tab to amend · ctrl+e to explain`).
fn is_footer(line: &str) -> bool {
    line.contains("Esc")
}

/// Match the rendered dialog in `lines` (KTD3). Abstains (`None`) on any
/// deviation from the supported shape — see the module doc.
fn parse_interaction(lines: &[String]) -> Option<ScreenInteraction> {
    // Every option-shaped line in the grid.
    let opts: Vec<OptionLine> = lines
        .iter()
        .enumerate()
        .filter_map(|(i, l)| option_line(i, l))
        .collect();
    if opts.len() < 2 {
        return None;
    }
    // One block, digits exactly 1..=N in order (R4: no renumbering, no
    // gap-closing — a second block or odd numbering means we cannot know what
    // a digit keypress selects).
    for (i, o) in opts.iter().enumerate() {
        if o.digit != (i + 1) as u32 {
            return None;
        }
    }
    // Exactly one cursor.
    let cursor_at = {
        let cursored: Vec<usize> = opts
            .iter()
            .enumerate()
            .filter(|(_, o)| o.cursored)
            .map(|(i, _)| i)
            .collect();
        if cursored.len() != 1 {
            return None;
        }
        cursored[0]
    };
    // The block must not start on the grid's first row — that smells of
    // options truncated by the ring/scroll (R3).
    let first_row = opts[0].row;
    if first_row == 0 {
        return None;
    }
    // Gap rows between options must be blank, rules, or indented continuation
    // text (which folds into the preceding option's description). Anything
    // else is a shape we don't know.
    let mut options: Vec<ScreenOption> = Vec::new();
    for (i, o) in opts.iter().enumerate() {
        let end = opts.get(i + 1).map(|n| n.row).unwrap_or(o.row + 1);
        let mut description: Vec<&str> = Vec::new();
        for r in o.row + 1..end {
            let line = &lines[r];
            let t = line.trim();
            if t.is_empty() || is_rule(line) {
                continue;
            }
            let line_indent = line.len() - line.trim_start().len();
            if line_indent > o.indent {
                description.push(t);
            } else {
                return None; // unindented interloper inside the block
            }
        }
        options.push(ScreenOption {
            digit: o.digit,
            label: o.label.clone(),
            description: description.join(" "),
        });
    }
    if options.iter().any(|o| o.label.is_empty()) {
        return None;
    }
    // A footer with an Esc hint within a few rows below the last option.
    let last_row = opts.last().unwrap().row;
    let footer_ok = lines
        .iter()
        .skip(last_row + 1)
        .take(4)
        .any(|l| is_footer(l));
    if !footer_ok {
        return None;
    }
    // The question: contiguous non-blank, non-rule lines immediately above the
    // block (skipping blanks between them and the options), read top→down.
    let mut q_end = first_row; // exclusive
    while q_end > 0 && lines[q_end - 1].trim().is_empty() {
        q_end -= 1;
    }
    let mut q_start = q_end;
    while q_start > 0 {
        let l = &lines[q_start - 1];
        if l.trim().is_empty() || is_rule(l) || option_line(q_start - 1, l).is_some() {
            break;
        }
        q_start -= 1;
    }
    if q_start == q_end {
        return None; // no question text — not a dialog we understand
    }
    let question = lines[q_start..q_end]
        .iter()
        .map(|l| l.trim())
        .collect::<Vec<_>>()
        .join(" ");

    // Kind: the permission dialog leads its options with "Do you want …".
    if question.starts_with("Do you want") {
        // Context: the box body above the question — title + request lines,
        // up to a blank-run/rule boundary or the grid top.
        let mut ctx: Vec<String> = Vec::new();
        let mut r = q_start;
        while r > 0 {
            let l = &lines[r - 1];
            if is_rule(l) {
                break;
            }
            let t = l.trim();
            if !t.is_empty() {
                ctx.push(strip_frame(t));
            }
            r -= 1;
        }
        ctx.reverse();
        return Some(ScreenInteraction {
            kind: ScreenKind::Permission,
            question,
            header: String::new(),
            context: ctx,
            options,
            cursor_at,
        });
    }

    // Choice picker: the header chip sits above the question, past one blank
    // run — a short line, often led by a checkbox/tab glyph.
    let mut h_end = q_start;
    while h_end > 0 && lines[h_end - 1].trim().is_empty() {
        h_end -= 1;
    }
    let header = if h_end > 0 && h_end != q_start {
        let l = lines[h_end - 1].trim();
        (!is_rule(&lines[h_end - 1]))
            .then(|| l.trim_start_matches(['☐', '☑', '✓', '✔']).trim().to_string())
            .unwrap_or_default()
    } else {
        String::new()
    };
    Some(ScreenInteraction {
        kind: ScreenKind::Choice,
        question,
        header,
        context: Vec::new(),
        options,
        cursor_at,
    })
}

/// Strip box-drawing frame characters from a context line's edges.
fn strip_frame(s: &str) -> String {
    s.trim_matches(|c: char| matches!(c, '│' | '┃' | '║' | ' ')).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real captured renders (Claude Code 2.1.206, 2026-07-10): an
    /// AskUserQuestion picker and a Bash permission dialog, each at 80 and 120
    /// columns, raw PTY bytes from spawn through the settled dialog. The
    /// captures are scrubbed byte-for-byte: the user's name is swapped for a
    /// same-length placeholder (`Alex` / `Alex Morgan`) and the MCP status
    /// notice is blanked, so every cursor move and row width is untouched.
    const ASK_80: &[u8] = include_bytes!("../../tests/fixtures/screen/ask-80.raw");
    const ASK_120: &[u8] = include_bytes!("../../tests/fixtures/screen/ask-120.raw");
    const PERM_80: &[u8] = include_bytes!("../../tests/fixtures/screen/perm-80.raw");
    const PERM_120: &[u8] = include_bytes!("../../tests/fixtures/screen/perm-120.raw");
    /// Real captured renders (Claude Code 2.1.207, 2026-07-11,
    /// fix-feed-question-detection-gaps): an AskUserQuestion picker RE-ASKED
    /// after the user declined the first ask — the reported bug's exact shape.
    /// The decline collapses the old picker to a digit-free summary line
    /// ("User declined to answer questions · What is your name? (…)"), so the
    /// fresh picker is the only numbered block on the grid.
    const ASK_DECLINED_REASK_80: &[u8] =
        include_bytes!("../../tests/fixtures/screen/ask-declined-reask-80.raw");
    /// The same session immediately after the decline, settled back at the
    /// idle input box — no dialog on screen. The widened fallback gate parses
    /// quiet screens without a corroborator, so this abstention is
    /// load-bearing: it is what keeps an answered/declined agent from being
    /// reported as still blocked.
    const ASK_DECLINED_IDLE_80: &[u8] =
        include_bytes!("../../tests/fixtures/screen/ask-declined-idle-80.raw");

    // Audit-remediation U8/KTD8: a 65535-row cursor jump must not transiently
    // allocate ~65k rows — the grid stays within MAX_GRID_ROWS at all times
    // (the clamp runs before the row-push loop) and rendering proceeds
    // unsurprised with the post-jump write on the final row.
    #[test]
    fn a_huge_cursor_jump_never_allocates_past_the_row_cap() {
        for seq in ["\x1b[65535B", "\x1b[65535e", "\x1b[65535E"] {
            let bytes = format!("hello{seq}world");
            let grid = render_tail(bytes.as_bytes(), 80);
            assert!(!grid.surprised, "{seq:?} is a supported sequence");
            assert!(
                grid.lines.len() <= MAX_GRID_ROWS,
                "{seq:?} grew the grid to {} rows",
                grid.lines.len()
            );
            let text = grid.text_lines();
            assert!(
                text.last().unwrap().contains("world"),
                "post-jump write lands on the tail row"
            );
        }
        // Stacked jumps stay bounded too (row is astronomically past the grid
        // by the time the next glyph forces ensure_row).
        let mut bytes: Vec<u8> = Vec::new();
        for _ in 0..10 {
            bytes.extend_from_slice(b"\x1b[65535B");
        }
        bytes.extend_from_slice(b"x");
        let grid = render_tail(&bytes, 80);
        assert!(grid.lines.len() <= MAX_GRID_ROWS);
        assert!(grid.text_lines().last().unwrap().contains('x'));
    }

    #[test]
    fn parses_the_real_ask_picker_at_80_cols() {
        let p = parse_screen_interaction(ASK_80, 80).expect("parses");
        assert_eq!(p.kind, ScreenKind::Choice);
        assert_eq!(p.question, "Which color do you prefer?");
        assert_eq!(p.header, "Color preference");
        // Digit fidelity (R4): the picker renders the two spec'd options PLUS
        // its own "Type something." and "Chat about this" extras — all four
        // are what the digits actually select, so all four are exposed.
        let digits: Vec<u32> = p.options.iter().map(|o| o.digit).collect();
        assert_eq!(digits, vec![1, 2, 3, 4]);
        assert_eq!(p.options[0].label, "Red");
        assert_eq!(p.options[0].description, "Warm and bold");
        assert_eq!(p.options[1].label, "Blue");
        assert_eq!(p.options[1].description, "Cool and calm");
        assert_eq!(p.options[2].label, "Type something.");
        assert_eq!(p.options[3].label, "Chat about this");
        assert_eq!(p.cursor_at, 0);
    }

    #[test]
    fn parses_the_real_ask_picker_at_120_cols() {
        let p = parse_screen_interaction(ASK_120, 120).expect("parses");
        assert_eq!(p.kind, ScreenKind::Choice);
        assert_eq!(p.question, "Which color do you prefer?");
        assert_eq!(p.options.len(), 4);
        assert_eq!(p.options[0].label, "Red");
        assert_eq!(p.options[1].label, "Blue");
    }

    #[test]
    fn parses_the_real_reasked_picker_after_a_decline() {
        let p = parse_screen_interaction(ASK_DECLINED_REASK_80, 80).expect("parses");
        assert_eq!(p.kind, ScreenKind::Choice);
        assert_eq!(p.question, "What is your name?");
        assert_eq!(p.header, "Name");
        // The collapsed first ask left no numbered rows, so the block is
        // exactly the re-asked picker: 3 authored options + the picker's own
        // "Type something." and "Chat about this" extras.
        let digits: Vec<u32> = p.options.iter().map(|o| o.digit).collect();
        assert_eq!(digits, vec![1, 2, 3, 4, 5]);
        assert_eq!(p.options[0].label, "Alex");
        assert_eq!(p.options[1].label, "Alex Morgan");
        assert_eq!(p.options[2].label, "Something else");
        assert_eq!(p.options[3].label, "Type something.");
        assert_eq!(p.options[4].label, "Chat about this");
        assert_eq!(p.cursor_at, 0);
    }

    #[test]
    fn abstains_on_the_real_post_decline_idle_screen() {
        assert_eq!(parse_screen_interaction(ASK_DECLINED_IDLE_80, 80), None);
    }

    #[test]
    fn parses_the_real_permission_dialog_at_80_cols() {
        let p = parse_screen_interaction(PERM_80, 80).expect("parses");
        assert_eq!(p.kind, ScreenKind::Permission);
        assert_eq!(p.question, "Do you want to proceed?");
        let digits: Vec<u32> = p.options.iter().map(|o| o.digit).collect();
        assert_eq!(digits, vec![1, 2, 3]);
        assert_eq!(p.options[0].label, "Yes");
        assert!(p.options[2].label.starts_with("No"));
        assert_eq!(p.cursor_at, 0);
        // The box body rides as context: the dialog title then the command.
        assert!(p.context.iter().any(|l| l.contains("Bash command")), "{:?}", p.context);
        assert!(
            p.context.iter().any(|l| l.contains("rm -f leftover.txt && date")),
            "{:?}",
            p.context
        );
    }

    #[test]
    fn parses_the_real_permission_dialog_at_120_cols() {
        let p = parse_screen_interaction(PERM_120, 120).expect("parses");
        assert_eq!(p.kind, ScreenKind::Permission);
        assert_eq!(p.options.len(), 3);
        assert_eq!(p.options[0].label, "Yes");
    }

    // ---- adversarial fixtures: every one of these must abstain (R3) ---------

    /// A hand-built minimal picker render (what a well-formed synthetic dialog
    /// looks like to the grid) — the base the adversarial cases mutate.
    fn synthetic(lines: &[&str]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(b"\x1b[H\x1b[2J");
        for l in lines {
            out.extend_from_slice(l.as_bytes());
            out.extend_from_slice(b"\r\n");
        }
        out
    }

    const GOOD: &[&str] = &[
        "some earlier output",
        "", // real dialogs always separate from prior output (header/blank)
        "Which color do you prefer?",
        "",
        "\u{276f} 1. Red",
        "  2. Blue",
        "",
        "Enter to select \u{b7} Esc to cancel",
    ];

    #[test]
    fn synthetic_baseline_parses() {
        let p = parse_screen_interaction(&synthetic(GOOD), 80).expect("baseline parses");
        assert_eq!(p.kind, ScreenKind::Choice);
        assert_eq!(p.options.len(), 2);
        assert_eq!(p.question, "Which color do you prefer?");
    }

    #[test]
    fn a_bare_decstbm_reset_is_harmless_but_a_sub_region_taints() {
        // vte reports `ESC[r` (full reset — Ink startup hygiene) as one
        // defaulted 0-param; it must NOT taint (the pinned regression from
        // fix-feed-question-detection-gaps: every capture that includes spawn
        // bytes carries one). A real sub-region still does.
        let mut with_reset = b"\x1b[r".to_vec();
        with_reset.extend_from_slice(&synthetic(GOOD));
        assert!(
            parse_screen_interaction(&with_reset, 80).is_some(),
            "bare ESC[r must not taint"
        );
        let mut with_region = b"\x1b[1;20r".to_vec();
        with_region.extend_from_slice(&synthetic(GOOD));
        assert_eq!(
            parse_screen_interaction(&with_region, 80),
            None,
            "a scroll sub-region is unmodeled → abstain"
        );
    }

    #[test]
    fn abstains_when_numbering_does_not_start_at_one() {
        let lines = [
            "Which color do you prefer?",
            "",
            "\u{276f} 2. Red",
            "  3. Blue",
            "",
            "Enter to select \u{b7} Esc to cancel",
        ];
        assert_eq!(parse_screen_interaction(&synthetic(&lines), 80), None);
    }

    #[test]
    fn abstains_on_non_contiguous_digits() {
        let lines = [
            "Which color do you prefer?",
            "",
            "\u{276f} 1. Red",
            "  3. Blue",
            "",
            "Enter to select \u{b7} Esc to cancel",
        ];
        assert_eq!(parse_screen_interaction(&synthetic(&lines), 80), None);
    }

    #[test]
    fn abstains_on_two_numbered_blocks() {
        // A stale dialog's options linger below the live one: the digits run
        // 1..N twice, so the sequence check breaks — we cannot know which
        // block a keypress addresses.
        let lines = [
            "Which color do you prefer?",
            "",
            "\u{276f} 1. Red",
            "  2. Blue",
            "",
            "Enter to select \u{b7} Esc to cancel",
            "  1. Yes",
            "  2. No",
        ];
        assert_eq!(parse_screen_interaction(&synthetic(&lines), 80), None);
    }

    #[test]
    fn abstains_with_no_cursor_or_two_cursors() {
        let none = [
            "Which color do you prefer?",
            "",
            "  1. Red",
            "  2. Blue",
            "",
            "Enter to select \u{b7} Esc to cancel",
        ];
        assert_eq!(parse_screen_interaction(&synthetic(&none), 80), None);
        let two = [
            "Which color do you prefer?",
            "",
            "\u{276f} 1. Red",
            "\u{276f} 2. Blue",
            "",
            "Enter to select \u{b7} Esc to cancel",
        ];
        assert_eq!(parse_screen_interaction(&synthetic(&two), 80), None);
    }

    #[test]
    fn abstains_without_a_footer() {
        let lines = [
            "Which color do you prefer?",
            "",
            "\u{276f} 1. Red",
            "  2. Blue",
        ];
        assert_eq!(parse_screen_interaction(&synthetic(&lines), 80), None);
    }

    #[test]
    fn abstains_without_a_question_line() {
        let lines = [
            "",
            "",
            "\u{276f} 1. Red",
            "  2. Blue",
            "",
            "Enter to select \u{b7} Esc to cancel",
        ];
        assert_eq!(parse_screen_interaction(&synthetic(&lines), 80), None);
    }

    #[test]
    fn abstains_when_options_may_be_scrolled_off_the_top() {
        // The block starting on the grid's first row means content above it —
        // possibly options — was lost to the ring/scroll.
        let mut bytes = Vec::new();
        for l in [
            "\u{276f} 1. Red",
            "  2. Blue",
            "",
            "Enter to select \u{b7} Esc to cancel",
        ] {
            bytes.extend_from_slice(l.as_bytes());
            bytes.extend_from_slice(b"\r\n");
        }
        assert_eq!(parse_screen_interaction(&bytes, 80), None);
    }

    #[test]
    fn abstains_on_an_unindented_interloper_inside_the_block() {
        let lines = [
            "Which color do you prefer?",
            "",
            "\u{276f} 1. Red",
            "some stray unindented text",
            "  2. Blue",
            "",
            "Enter to select \u{b7} Esc to cancel",
        ];
        assert_eq!(parse_screen_interaction(&synthetic(&lines), 80), None);
    }

    #[test]
    fn abstains_on_a_surprising_control_sequence() {
        // An IL (insert line) — a layout mutation the grid doesn't model —
        // taints the render even though the text would otherwise parse.
        let mut bytes = synthetic(GOOD);
        bytes.extend_from_slice(b"\x1b[2L");
        assert_eq!(parse_screen_interaction(&bytes, 80), None);
    }

    #[test]
    fn abstains_on_free_text_prompts_and_plain_output() {
        // A shell prompt / plain build output: nothing option-shaped.
        let lines = [
            "$ cargo test",
            "   Compiling fly v0.1.0",
            "error[E0308]: mismatched types",
            "$",
        ];
        assert_eq!(parse_screen_interaction(&synthetic(&lines), 80), None);
        assert_eq!(parse_screen_interaction(b"", 80), None);
    }

    #[test]
    fn a_rule_inside_the_block_is_tolerated() {
        // The real picker draws a rule before its trailing "Chat about this"
        // extra — that exact shape must keep parsing (pinned by ASK_80 too;
        // this isolates the rule handling).
        let lines = [
            "Which color do you prefer?",
            "",
            "\u{276f} 1. Red",
            "  2. Blue",
            "\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}",
            "  3. Chat about this",
            "",
            "Enter to select \u{b7} Esc to cancel",
        ];
        let p = parse_screen_interaction(&synthetic(&lines), 80).expect("parses");
        assert_eq!(p.options.len(), 3);
    }

    #[test]
    fn grid_replay_survives_mid_sequence_ring_start() {
        // The ring may begin mid-escape-sequence; vte resyncs and the last
        // full repaint still wins. Chop the real fixture at an awkward point.
        let cut = &ASK_80[137..]; // arbitrary odd offset into the preamble
        let p = parse_screen_interaction(cut, 80).expect("still parses after resync");
        assert_eq!(p.question, "Which color do you prefer?");
    }
}
