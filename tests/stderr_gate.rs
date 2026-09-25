//! The picker draws on stderr, so its styling must follow stderr — not stdout.
//!
//! # Why this is an integration test rather than a unit one
//!
//! `console` keeps its two "are colours on" switches in process-wide `OnceLock<AtomicBool>`
//! statics, one per stream. Proving the wiring means writing BOTH of them: stderr on, stdout off.
//! Every test in a `#[cfg(test)] mod tests` shares one process, so doing that beside the unit
//! tests would pin stdout's switch `false` for the whole binary — harmless today, because nothing
//! stdout-bound is styled, and a horrible order-dependent failure the first time something is
//! (a `--color` flag, a warning, colourised output for a tty).
//!
//! Cargo gives every file under `tests/` its own process, so the mutation cannot reach anything
//! else. That is the entire reason this lives here, and it is why the unit tests may keep to the
//! single-gate helper that cannot conflict with anything.
//!
//! # The bug it exists to catch
//!
//! A bare `console::Style` is gated on STDOUT, whatever stream its bytes end up on. This crate
//! writes its frames to stderr precisely so stdout stays free for the palette — so under the
//! usage the binary advertises, `arterminal art.txt > palette.txt`, a default-built style
//! consults the redirected file, concludes there is no terminal, and emits nothing. A colour
//! picker in no colour. Attributes share that gate, so `.dim()` and `.bold()` go with the colours.
//!
//! No unit test can see this: under a test harness neither stream is a terminal, so both gates
//! read false and the two code paths are indistinguishable. It was found by driving the real
//! binary under a pty and counting zero truecolor escapes.

use arterminal::ui::render;
use arterminal::{Canvas, Focus, Palette, Picker, Rgb};

#[test]
fn every_style_follows_stderr_and_not_a_redirected_stdout() {
    console::set_colors_enabled_stderr(true);
    console::set_colors_enabled(false); // as if `arterminal art.txt > palette.txt`

    let mut palette = Palette::new();
    palette.push("sky", Rgb::new(30, 144, 255)).expect("a fresh palette");
    let mut picker =
        Picker::new(Canvas::from_text("ab\ncd\nef\ngh").expect("valid")).with_palette(palette);
    assert!(picker.set_focus(Focus::Cell { x: 0, y: 0 }));
    // Deliberately too short for the art, so the window scrolls and the status row has to say
    // which rows it is showing — the one piece of text only a short terminal draws.
    let lines = render(&picker, 200, 10);

    assert_eq!(
        lines.len(),
        10,
        "swatch, button, gap, border, two art rows, status, 3 hints: {lines:?}"
    );

    assert!(lines[0].contains("\x1b[48;2;30;144;255m"), "swatch lost its colour: {:?}", lines[0]);
    assert!(lines[3].contains("\x1b[48;2;110;110;110m"), "border lost its grey: {:?}", lines[3]);
    assert!(lines[4].contains("\x1b[7m"), "cursor lost its inversion: {:?}", lines[4]);

    // Attributes sit inside the same gate as the colours, so these die the same death.
    let status = &lines[6];
    assert!(status.contains("rows 1-2 of 4"), "expected where the window is: {status:?}");
    assert!(status.contains("\x1b[2m"), "status lost its dim: {status:?}");

    let file = &lines[7];
    assert!(file.contains("esc"), "expected the file's hint line: {file:?}");
    assert!(file.contains("\x1b[2m"), "its dots lost their dim: {file:?}");
    assert!(file.contains("\x1b[33m"), "its actions lost their colour: {file:?}");
}
