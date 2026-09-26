//! Terminal ASCII art with colour.
//!
//! A library first: every feature the bundled `arterminal` binary has is a function here, so a
//! program embedding this crate is never the second-class caller.
//!
//! # The pieces
//!
//! A [`Canvas`] is a rectangular grid of [`Cell`]s — one character each, loaded from plain text.
//! A [`Palette`] is the colours that drawing may use, each under a unique name and each a
//! unique colour. A cell holds its [`Rgb`] directly, which is what makes a canvas printable as it
//! stands; editing a swatch still recolours the drawing, by rewriting every cell holding that
//! colour — see [`Picker::set_swatch_color`]. A [`Picker`] puts the two together and lets someone
//! walk over both with the arrow keys.
//!
//! ```no_run
//! use arterminal::Picker;
//!
//! // Art, colours and palette all come from the one file, and go back to it on Ctrl+S.
//! let mut picker = Picker::open("art.txt")?;
//! arterminal::run(&mut picker)?;
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! Or built in code, with nowhere to save to unless told:
//!
//! ```no_run
//! use arterminal::{Canvas, Picker, Palette, Rgb};
//!
//! let mut palette = Palette::new();
//! palette.push("sky", Rgb::from_hex("#1e90ff")?)?;
//! let mut picker = Picker::new(Canvas::from_text("⣿⡇\n⢹⡇")?).with_palette(palette);
//! arterminal::run(&mut picker)?;
//! picker.save_to("out.txt")?;
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! # Driving it yourself
//!
//! [`run`] takes the terminal over. A program that already owns its screen can instead call
//! [`ui::render`] for the lines and [`ui::apply`] for the key handling, and composite the result
//! wherever it likes. Both are pure functions over state, which is also why the whole picker is
//! testable without a terminal.
//!
//! ```
//! use arterminal::{Canvas, Focus, KeyCode, Picker};
//! use arterminal::ui::{apply, render, Action};
//!
//! let mut picker = Picker::new(Canvas::from_text("ab\ncd")?);
//! assert_eq!(picker.focus(), Focus::Add, "no swatches yet, so the cursor starts on [+]");
//!
//! assert_eq!(apply(&mut picker, KeyCode::Enter), Action::Redraw);
//! assert!(picker.dial().is_some(), "enter on [+] opened the colour dial");
//! apply(&mut picker, KeyCode::Up); // a degree round the wheel
//! apply(&mut picker, KeyCode::Enter);
//! assert_eq!(picker.palette().len(), 1, "and enter there added the colour");
//!
//! let lines = render(&picker, 40, 12);
//! assert!(lines.iter().any(|line| line.contains("[+]")));
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! # Terminal support
//!
//! A colour is either 24-bit, emitted as such, or one of the terminal's own 256 palette SLOTS — see
//! [`Ink`] — kept as a slot and drawn through the terminal, so it looks as this terminal shows it.
//! New swatches are always 24-bit. Terminals that only do the 256-colour palette will show the
//! nearest thing they can to a 24-bit colour; down-converting for them is not yet implemented,
//! and when it is it will happen at the point of drawing, never by narrowing what an [`Rgb`] can
//! hold.

pub mod canvas;
pub mod color;
pub mod cursor;
pub mod dial;
pub mod document;
pub mod elevate;
pub mod keys;
pub mod palette;
pub mod ui;

mod devices;
mod input;
mod paint;

pub use crate::canvas::{At, Canvas, CanvasError, Cell, LoadCause, LoadError};
pub use crate::color::{ColorParseError, Hsb, Ink, Rgb, Rng};
pub use crate::cursor::{Dir, Focus};
pub use crate::devices::InputDevices;
pub use crate::dial::{Channel, Dial};
pub use crate::document::{Document, DocumentError};
pub use crate::keys::{KeyCode, KeyEvent, KeyKind, Mods};
pub use crate::palette::{Derivation, HsbOffset, Palette, PaletteError, Recolour, Swatch};
pub use crate::ui::{run, run_with_devices, Action, Outcome, Picker};
