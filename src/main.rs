//! The standalone runner: the same picker the library serves, from a shell.
//!
//! One argument, the file to edit, and optionally `--su` — see `arterminal::elevate`. The picker
//! draws on stderr, and everything it produces goes back into that file on Ctrl+S — so stdout
//! carries nothing, and a shell pipeline around this program is a pipeline around a silent one.
//! Exit codes: 0 the picker ran, 2 the file or the terminal was unusable — and under `--su`, a
//! sudo that refused passes its own status through.

use arterminal::elevate::Elevation;
use arterminal::{Outcome, Picker};

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
      ^S save · ^Z undo · ^Y redo
      ↑↓←→ move · space/enter pick a swatch, add one on [+], or hold on a cell to paint
      backspace hold to erase · b / del toggle painting / erasing · F2 recolour & rename
      i pick up the colour under the cursor · [ ] shrink / grow the pen
      F6 split into canvas and art · shift+F6 side by side · c art cursor in the split
      shift+arrows scroll · page up/down jump a screen · F5 redraw

Holding a key needs its release reported. Terminals that speak the kitty keyboard protocol report
it (kitty, WezTerm, foot, Ghostty, …). Elsewhere, on Linux, the input devices can — for root and
the input group. Either run with --su, which asks sudo each run, or join the group once:

  sudo usermod -aG input \"$USER\"      (then log out and back in)

The group lets EVERY program you run read every key typed on the machine, passwords included.
arterminal only asks which keys are down, but the group grants the rest regardless. With neither,
space and backspace tap one cell, and b / del still drag. With --su, a refused sudo exits with
sudo's own status.";

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

    let mut picker = match Picker::open(invocation.path) {
        Ok(picker) => picker,
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
    fn a_double_dash_ends_the_flags() {
        let odd = args(&["--", "-odd.txt"]);
        assert_eq!(parse(&odd), Ok(Some(Invocation { path: "-odd.txt", su: false })));
        let su_then = args(&["--su", "--", "--su"]);
        assert_eq!(parse(&su_then), Ok(Some(Invocation { path: "--su", su: true })));
    }
}
