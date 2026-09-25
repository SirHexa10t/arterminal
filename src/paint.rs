//! Getting a frame onto the terminal without flicker.
//!
//! # Provenance
//!
//! [`frame`] and [`paint`] are a port of `_frame` / `_paint` from `src/ui.rs` of the sibling
//! project `terminal_choice` (commit `4f7a222`). Both projects are GPL-3.0-only and share an
//! owner, so the copy is clean; this notice is here because a copy without one is how two files
//! quietly become two different files.
//!
//! Ported rather than shared, deliberately. Depending on `terminal_choice` would point an art
//! editor at a form library — the wrong direction — and extracting a third crate assumes the
//! interface has settled, which this one has not: [`height_budget`] is new here, and a scrolling
//! viewport will change the shape again. Revisit extraction once the viewport lands.
//!
//! # Why the flicker goes away
//!
//! Two rules, and they fix different halves of the problem.
//!
//! ONE WRITE, because `Term::stderr()` is unbuffered: every `write_line`, every `clear_line`,
//! every cursor move is its own `write(2)`. The obvious repaint — clear the old block, print the
//! new one — costs `3n + 2` of them, which is 122 syscalls for a 40-line frame. A terminal
//! renders as those arrive, so the user watches the block blank line by line and fill line by
//! line. That is the flicker. A buffered `Term` plus one flush makes the whole frame a single
//! write, and the terminal has nothing to show halfway through.
//!
//! NO BLANK STATE, because "erase n lines, then draw n lines" describes an empty screen even
//! when it arrives in one piece. Each line is instead overwritten where it stands and followed
//! by `\x1b[K`, which clears only whatever the old line left to the right of it. Nothing is ever
//! blank, so nothing can be caught blank.

use console::Term;

/// Paint `lines` over the frame already on screen, which was `previous` lines tall.
///
/// The cursor starts and finishes immediately after the block, which is what lets the next call
/// find it by moving up `previous` lines. `term` must be buffered — see the module docs; an
/// unbuffered one still draws the right thing, just one syscall at a time, which is the bug.
pub(crate) fn paint(term: &Term, lines: &[String], previous: usize) -> std::io::Result<()> {
    term.write_str(&frame(lines, previous))?;
    term.flush()
}

/// The bytes one repaint sends, as a single string — pure, so the escape arithmetic can be
/// tested without a terminal, like everything else that decides what appears.
///
/// Built whole before anything is written, which makes the single write a property of this
/// function rather than a hope about the buffer underneath it.
///
/// The cursor ends `lines.len()` rows below where the old block began — that is, immediately
/// after the new one — which is the invariant the next call depends on. An empty frame therefore
/// leaves it exactly where the block started, having erased the lot.
pub(crate) fn frame(lines: &[String], previous: usize) -> String {
    let mut out = String::new();
    if previous > 0 {
        out.push_str(&format!("\x1b[{previous}A"));
    }
    for line in lines {
        out.push_str(line);
        // Erase only what the old line left to the RIGHT of this one. Nothing is ever blanked
        // first, so no repaint can be caught halfway.
        out.push_str("\x1b[K\n");
    }
    // A frame that SHRANK leaves rows of the old one below it. Wipe those, then come back to sit
    // just under the new frame.
    let surplus = previous.saturating_sub(lines.len());
    for _ in 0..surplus {
        out.push_str("\x1b[K\n");
    }
    if surplus > 0 {
        out.push_str(&format!("\x1b[{surplus}A"));
    }
    out
}

/// How many lines a frame may occupy on a terminal `rows` high: one fewer than you would think.
///
/// A painted frame terminates EVERY line with `\x1b[K\n`, the last one included, so after painting
/// a block `h` lines tall the cursor sits on row `h + 1`. A block of exactly `rows` height would
/// therefore emit its final newline on the bottom row, the terminal would scroll to make room,
/// and the block's top row would be gone.
///
/// That matters more than one row of screen space, because the damage is permanent rather than
/// cosmetic. `\x1b[nA` CLAMPS at the top margin — it cannot un-scroll. While the whole block
/// fits, scrolling is harmless: the block and the cursor move up together, so the relative step
/// still lands. The moment that step would cross row 1 it stops landing, and every repaint after
/// it is wrong. The failure is a cliff, not a drift.
///
/// The sibling this module came from never hit it: its `render` clips lines to the terminal's
/// WIDTH, with a comment explaining that a wrapped line would break exactly this arithmetic —
/// the invariant was already known, and enforced in one dimension only. Forms are short; a
/// canvas is not.
pub fn height_budget(rows: usize) -> usize {
    rows.saturating_sub(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Net vertical movement of a frame: rows stepped down, minus rows stepped up.
    ///
    /// Counts the real escape bytes rather than re-deriving them from the inputs — a second
    /// implementation of the arithmetic would agree with the first right up until it mattered.
    fn drift(frame: &str) -> isize {
        let downs = frame.matches('\n').count() as isize;
        let ups: isize = frame
            .split("\x1b[")
            .filter_map(|part| part.split_once('A'))
            .filter(|(digits, _)| !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()))
            .filter_map(|(digits, _)| digits.parse::<isize>().ok())
            .sum();
        downs - ups
    }

    /// The whole repaint is ONE string, and it never blanks anything before drawing it. Those two
    /// together are what stopped the flicker.
    #[test]
    fn a_repaint_is_one_write_that_overwrites_rather_than_clearing() {
        let frame = frame(&["one".into(), "two".into()], 2);
        assert_eq!(frame, "\x1b[2Aone\x1b[K\ntwo\x1b[K\n");
        assert!(!frame.contains("\x1b[2K"), "nothing is blanked whole: {frame:?}");
        assert!(!frame.contains("\x1b[2J"), "and the screen is never cleared: {frame:?}");
    }

    /// The first paint has nothing above it to step back over.
    #[test]
    fn the_first_frame_does_not_move_the_cursor_up() {
        let frame = frame(&["only".into()], 0);
        assert_eq!(frame, "only\x1b[K\n");
        assert_eq!(drift(&frame), 1, "one line drawn, cursor one below where it began");
    }

    /// A frame that shrank must wipe what it no longer covers, or the tail of the old one stays
    /// on screen underneath the new.
    #[test]
    fn a_shorter_frame_erases_the_rows_it_gave_up() {
        let frame = frame(&["kept".into()], 4);
        assert_eq!(frame, "\x1b[4Akept\x1b[K\n\x1b[K\n\x1b[K\n\x1b[K\n\x1b[3A");
        assert_eq!(drift(&frame), -3, "four rows became one: three higher than before");
    }

    /// Growing needs no wiping: the new rows land on ground the old frame never held.
    #[test]
    fn a_longer_frame_just_writes_the_extra_rows() {
        let frame = frame(&["a".into(), "b".into(), "c".into()], 1);
        assert_eq!(frame, "\x1b[1Aa\x1b[K\nb\x1b[K\nc\x1b[K\n");
        assert_eq!(drift(&frame), 2);
    }

    /// The invariant every repaint depends on: afterwards the cursor sits immediately below the
    /// frame just drawn, so the next call finds the block by stepping up its own height. Checked
    /// across every shape, because getting it wrong by one drifts the picker down the screen a
    /// row per keystroke — which is the other way a terminal UI flickers.
    #[test]
    fn the_cursor_always_lands_just_under_the_new_frame() {
        for previous in 0..6 {
            for height in 0..6 {
                let lines: Vec<String> = (0..height).map(|n| format!("line {n}")).collect();
                let frame = frame(&lines, previous);
                assert_eq!(
                    drift(&frame),
                    height as isize - previous as isize,
                    "{height} lines over {previous}: {frame:?}"
                );
            }
        }
    }

    /// Teardown is the empty frame: everything erased, cursor back where the block began.
    #[test]
    fn an_empty_frame_removes_the_block_and_rewinds() {
        let frame = frame(&[], 3);
        assert_eq!(frame, "\x1b[3A\x1b[K\n\x1b[K\n\x1b[K\n\x1b[3A");
        assert_eq!(drift(&frame), -3, "back to where the block started");
        assert!(!frame.contains("line"), "and nothing drawn");
    }

    /// The off-by-one this function exists for. A frame filling every row of the terminal would
    /// scroll on its own last newline; the budget is therefore one short of the height.
    #[test]
    fn the_height_budget_leaves_a_row_for_the_trailing_newline() {
        assert_eq!(height_budget(24), 23);
        assert_eq!(height_budget(1), 0, "a one-row terminal has room for no frame at all");
        assert_eq!(height_budget(0), 0, "and a zero-row one cannot underflow");
    }

    /// The budget's whole claim, stated as the property rather than as arithmetic: a frame drawn
    /// within it emits its last newline with a row still to spare, so nothing scrolls.
    #[test]
    fn a_frame_within_budget_never_needs_the_terminals_last_row() {
        for rows in 1..40 {
            let height = height_budget(rows);
            let lines: Vec<String> = (0..height).map(|n| format!("line {n}")).collect();
            let newlines = frame(&lines, 0).matches('\n').count();
            assert!(
                newlines < rows,
                "{height} lines on a {rows}-row terminal emitted {newlines} newlines — the last \
                 would scroll the block's top row away"
            );
        }
    }
}
