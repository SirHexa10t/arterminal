//! With styling off — output redirected, or `NO_COLOR` set — the dial's strips are left off.
//!
//! # Why a file of its own
//!
//! `console` keeps its "are colours on" switches in process-wide statics, and the unit tests pin
//! stderr's ON, so switching it off beside them would race every test that looks for an escape.
//! Cargo gives each file under `tests/` a process of its own; `tests/stderr_gate.rs` explains the
//! same arrangement in full.

use arterminal::ui::{apply, render};
use arterminal::{Canvas, KeyCode, Palette, Picker, Rgb};

/// A strip of colour drawn in no colour would show nothing, and still cut into the lines beneath
/// it. So with styling off there is none: the frame while dialling is the dial's own lines, and
/// every other line whole.
#[test]
fn with_styling_off_the_strips_are_left_off() {
    console::set_colors_enabled_stderr(false);
    let long = format!("long {}", "x".repeat(80));
    let mut palette = Palette::new();
    palette.push("sky", Rgb::new(30, 144, 255)).expect("a fresh palette");
    palette.push(long.clone(), Rgb::new(200, 40, 40)).expect("a second colour");
    let art = ["ab"; 12].join("\n");
    let mut picker = Picker::new(Canvas::from_text(&art).expect("valid")).with_palette(palette);
    apply(&mut picker, KeyCode::F(2));
    assert!(picker.dial().is_some(), "F2 on the first swatch opened the dial");

    let lines = render(&picker, 140, 40);
    assert!(lines.iter().any(|line| line.contains("H:")), "the dial is drawn: {lines:?}");
    assert!(lines.iter().all(|line| !line.contains('\x1b')), "nothing styled: {lines:?}");
    let label = lines.iter().find(|line| line.contains("# long")).expect("the long label's row");
    assert!(label.ends_with(&long), "the label runs on, uncut: {label:?}");
    assert!(lines.iter().all(|line| !line.contains("H   S   B")), "no strips: {lines:?}");
}
