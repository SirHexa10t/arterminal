//! The standalone runner: the same picker the library serves, from a shell.
//!
//! One argument, the file to edit. The picker draws on stderr, and everything it produces goes
//! back into that file on Ctrl+S — so stdout carries nothing, and a shell pipeline around this
//! program is a pipeline around a silent one. Exit codes: 0 the picker ran, 2 the file or the
//! terminal was unusable.

use arterminal::Picker;

const USAGE: &str = "\
arterminal — colour a piece of ASCII art in the terminal

  arterminal ART.txt

ART.txt is text: one line per row, one character per cell, colours (if any) written inline as
terminal escapes, and the palette after the art. A plain text file is a valid document with no
colours and an empty palette. Every glyph must be exactly one terminal column wide, so no tabs,
no emoji, no CJK — braille, block elements and box drawing are fine.

Keys: ↑↓←→ move · space/enter pick a swatch or paint with it · backspace erase
      ^S save · ^Z undo · ^Y redo · esc/^X close (twice if unsaved) · ^C close at once";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let path = match args.as_slice() {
        [path] if !path.starts_with('-') => path,
        [] | [_] => {
            println!("{USAGE}");
            return;
        }
        _ => fail("one art file at a time"),
    };

    let mut picker = match Picker::open(path) {
        Ok(picker) => picker,
        Err(err) => fail(&err.to_string()),
    };
    if let Err(err) = arterminal::run(&mut picker) {
        fail(&err.to_string());
    }
}

/// Report why nothing could be done, and stop. Exit 2 is "the file or the terminal was unusable",
/// as distinct from a picker that ran.
fn fail(why: &str) -> ! {
    eprintln!("arterminal: {why}");
    std::process::exit(2)
}
