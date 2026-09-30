//! The standalone runner: the same picker the library serves, from a shell.
//!
//! One argument, the file to edit, and optionally `--su` — see `arterminal::elevate`. The picker
//! draws on stderr, and everything it produces goes back into that file on Ctrl+S — so stdout
//! carries nothing, and a shell pipeline around this program is a pipeline around a silent one.
//! Before the picker opens it may ask one question, on the terminal while it is still in its
//! ordinary state: whether to open a very large drawing. Exit codes: 0 the picker ran, or the person declined to open the file; 2 the
//! file or the terminal was unusable — and under `--su`, a sudo that refused passes its own status
//! through.

use arterminal::elevate::Elevation;
use arterminal::{document, Outcome, Picker};

const USAGE: &str = "\
arterminal — colour a piece of ASCII art in the terminal

  arterminal [--su] ART.txt

ART.txt is text: one line per row, one character per cell, colours (if any) written inline as
terminal escapes, and the palette after the art. A plain text file is a valid document with no
colours and an empty palette. Every glyph must be exactly one terminal column wide, so no tabs,
no emoji, no CJK — braille, block elements and box drawing are fine.

  --su   hold keys in any terminal: sudo opens the keyboards for this run only, and root is
         dropped as soon as they are open, before ART.txt is read. See below.

Keys: ^X/esc close (twice if unsaved) · ^C quit, unsaved work kept in ART.txt.arterminal.tmp
      ^S save · ^Z undo · shift+^Z redo (^Y where the terminal cannot tell it from ^Z)
      ↑↓←→ move · space/enter pick a swatch, add one on [+], or hold on a cell to paint
      backspace hold to erase · b / del toggle painting / erasing · F2 recolour & rename
      i pick up the colour under the cursor · [ ] shrink / grow the pen
      F6 split into canvas and art · shift+F6 side by side · c art cursor in the split
      shift+arrows scroll a picture bigger than the window · page up/down jump a screen
      F5 redraw
      on the colour dial ([+], F2): ←→ or tab pick H, S or B · ↑↓ turn it · page up/down by 10
      0-9 type the value · # and six hex digits type a colour · backspace takes a digit back
      space/enter keep the colour, then name it — one undo with the name · esc give it up

Holding a key needs its release reported. Terminals that speak the kitty keyboard protocol report
it (kitty, WezTerm, foot, Ghostty, …). Elsewhere, on Linux, the input devices can — for root and
the input group. Either run with --su, which asks sudo each run, or join the group once:

  sudo usermod -aG input \"$USER\"      (then log out and back in)

The group lets EVERY program you run read every key typed on the machine, passwords included.
arterminal only asks which keys are down, but the group grants the rest regardless. With neither,
space and backspace tap one cell, and b / del still drag. With --su, a refused sudo exits with
sudo's own status.

Before opening, arterminal asks about a drawing past 10,000 columns or rows or 10 million cells,
saying what it will cost in memory.

Files may use the terminal's 16- and 256-colour slots as well as 24-bit colours: slot colours load
as slot swatches, tagged in the palette, and are saved back as slots. New swatches are 24-bit.

Past 1 GB of memory, a red line at the bottom says how much of it is the image and how much the
undo history. Nothing is capped; reopening the file starts a fresh history.";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let invocation = match parse(arterminal::elevate::without_rerun_mark(&args)) {
        Ok(Some(invocation)) => invocation,
        Ok(None) => {
            println!("{USAGE}");
            return;
        }
        Err(why) => fail(&why),
    };

    // FIRST: before the art file is so much as looked at, and before anything touches the
    // terminal. Both are hard rules, argued in `arterminal::elevate`.
    let devices = match invocation.su {
        false => None,
        true => match arterminal::elevate::for_this_run() {
            Ok(Elevation::Finished(code)) => std::process::exit(code),
            Ok(Elevation::Proceed { devices, note }) => {
                if let Some(note) = note {
                    eprintln!("arterminal: {note}");
                }
                devices
            }
            Err(err) => fail(&format!("--su: {err}")),
        },
    };

    let mut picker = match open(invocation.path) {
        Ok(Some(picker)) => picker,
        Ok(None) => {
            eprintln!("arterminal: not opened");
            return;
        }
        Err(err) => fail(&err.to_string()),
    };
    match arterminal::run_with_devices(&mut picker, devices) {
        Ok(Outcome::Interrupted { salvaged: Some(kept) }) => {
            eprintln!("arterminal: unsaved changes kept in {}", kept.display());
        }
        Ok(_) => {}
        Err(err) => fail(&err.to_string()),
    }
}

/// Open `path`, asking first if it is very large — before the picker takes the terminal over.
/// `None` when the person declined.
///
/// Read once and MEASURED before anything is built: a single long line pads every row out to its
/// width, so a small file can need a great deal of memory, and this is the moment to say so.
fn open(path: &str) -> Result<Option<Picker>, arterminal::LoadError> {
    let text = document::read(path)?;
    let size = document::measure(&text);
    if size.is_large() && !confirm(&large_question(path, size)) {
        return Ok(None);
    }
    Picker::from_text(path, &text).map(Some)
}

/// The question before opening a drawing past one of the lines in [`document::Size`].
fn large_question(path: &str, size: document::Size) -> String {
    format!(
        "{path} is {} columns by {} rows, {} cells, and opening it takes about {} of memory. Open \
         it anyway?",
        thousands(size.width),
        thousands(size.height),
        thousands(size.cells()),
        arterminal::ui::readable_bytes(size.canvas_bytes()),
    )
}

/// `n` with its thousands marked: 50,000.
fn thousands(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (at, digit) in digits.chars().enumerate() {
        if at > 0 && (digits.len() - at).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

/// Ask a yes-or-no question on the terminal, while it is still in its ordinary state. Anything
/// but a clear yes is a no — and so is having no terminal to ask at.
fn confirm(question: &str) -> bool {
    use std::io::{BufRead, IsTerminal};
    eprint!("arterminal: {question} [y/N] ");
    let mut answer = String::new();
    let read = match std::io::stdin().is_terminal() {
        true => std::io::stdin().lock().read_line(&mut answer),
        false => match std::fs::File::open("/dev/tty") {
            Ok(tty) => std::io::BufReader::new(tty).read_line(&mut answer),
            Err(_) => {
                eprintln!();
                return false;
            }
        },
    };
    read.is_ok() && matches!(answer.trim(), "y" | "Y" | "yes" | "Yes" | "YES")
}

/// What the command line asks for.
#[derive(Debug, PartialEq, Eq)]
struct Invocation<'a> {
    path: &'a str,
    /// `--su`: sudo opens the keyboards for this run only — see `arterminal::elevate`.
    su: bool,
}

/// `Ok(None)` means "print the usage": asked for, or nothing given to open. Flags may come before
/// or after the file, and `--` ends them, for a file whose name starts with a dash.
fn parse(args: &[String]) -> Result<Option<Invocation<'_>>, String> {
    let (mut su, mut paths, mut flags) = (false, Vec::new(), true);
    for arg in args {
        match arg.as_str() {
            "--" if flags => flags = false,
            "-h" | "--help" if flags => return Ok(None),
            "--su" if flags => su = true,
            flag if flags && flag.starts_with('-') => {
                return Err(format!("unknown option {flag} (see --help)"));
            }
            path => paths.push(path),
        }
    }
    match paths.as_slice() {
        [] => Ok(None),
        [path] => Ok(Some(Invocation { path, su })),
        _ => Err("one art file at a time".to_string()),
    }
}

/// Report why nothing could be done, and stop. Exit 2 is "the file or the terminal was unusable",
/// as distinct from a picker that ran.
fn fail(why: &str) -> ! {
    eprintln!("arterminal: {why}");
    std::process::exit(2)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|arg| arg.to_string()).collect()
    }

    #[test]
    fn one_file_and_an_optional_su_either_side_of_it() {
        let plain = args(&["art.txt"]);
        assert_eq!(parse(&plain), Ok(Some(Invocation { path: "art.txt", su: false })));
        for order in [["--su", "art.txt"], ["art.txt", "--su"]] {
            let given = args(&order);
            assert_eq!(parse(&given), Ok(Some(Invocation { path: "art.txt", su: true })));
        }
    }

    #[test]
    fn help_or_nothing_to_open_prints_the_usage() {
        for list in [&[][..], &["--help"], &["-h", "art.txt"], &["--su"]] {
            assert_eq!(parse(&args(list)), Ok(None), "{list:?}");
        }
    }

    #[test]
    fn anything_else_is_refused_with_a_reason() {
        let unknown = parse(&args(&["-x", "art.txt"])).expect_err("refused");
        assert!(unknown.contains("unknown option -x"), "{unknown}");
        let two = parse(&args(&["a.txt", "b.txt"])).expect_err("refused");
        assert!(two.contains("one art file"), "{two}");
    }

    #[test]
    fn the_size_question_gives_the_size_and_what_it_costs() {
        let size = document::Size { width: 50_000, height: 3 };
        let question = large_question("art.txt", size);
        assert!(
            question.contains("art.txt is 50,000 columns by 3 rows, 150,000 cells"),
            "{question}"
        );
        assert!(question.contains("about 1.2 MB of memory"), "{question}");
        let square = large_question("big.txt", document::Size { width: 9_000, height: 9_000 });
        assert!(square.contains("81,000,000 cells") && square.contains("648.0 MB"), "{square}");
    }

    #[test]
    fn thousands_are_marked() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(1_234_567), "1,234,567");
    }

    #[test]
    fn a_double_dash_ends_the_flags() {
        let odd = args(&["--", "-odd.txt"]);
        assert_eq!(parse(&odd), Ok(Some(Invocation { path: "-odd.txt", su: false })));
        let su_then = args(&["--su", "--", "--su"]);
        assert_eq!(parse(&su_then), Ok(Some(Invocation { path: "--su", su: true })));
    }
}
