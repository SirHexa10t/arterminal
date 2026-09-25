//! One text file holding a drawing, its colours, and its palette.
//!
//! # The shape, and why
//!
//! The art comes first, as plain text with the colours written INLINE as terminal escapes — so
//! `cat` shows the drawing in colour, and so does anything else that prints the file. After it,
//! if there is anything to say, a marker line and then the palette, one swatch per line.
//!
//! ```text
//! ⣿⣿ESC[38;2;226;57;57m⡇ESC[0m
//! === arterminal palette ===
//! ember<TAB>#e23939
//! shade<TAB>#a02020<TAB>ember<TAB>0,0,-60
//! ```
//!
//! A file with no colour and no palette is exactly the plain text it started as, so a drawing
//! that was only ever looked at is saved byte for byte — the format costs nothing until it is
//! used. A file with colours but no palette loads too, and every colour found in it becomes a
//! swatch with a counted name, because the rule elsewhere is that every colour has one.
//!
//! The trailer is VISIBLE when the file is printed, which is a compromise made deliberately: the
//! ways of hiding it (an APC string, a sidecar file) each cost more than a few lines under the
//! art, and this one is easy to swap for another because nothing outside this module knows the
//! layout. Recolouring one swatch rewrites every line that used it, which is inherent to keeping
//! the colours inline and was accepted with eyes open.
//!
//! # What the loader tolerates
//!
//! More than [`render`] writes, on purpose — a file may have been made by hand. Only the
//! 24-bit foreground sequence (`ESC[38;2;r;g;b m`) sets a colour; `ESC[0m` and `ESC[39m` clear
//! it; every other sequence, bold and background and the 256-colour palette included, is passed
//! over and its text kept. Colour CARRIES across line ends, as it does on a terminal, so a file
//! that sets a colour once and never resets it reads the way it prints. Rows [`render`] writes
//! always reset at their end, so nothing this crate saved depends on that.

use crate::canvas::{Canvas, CanvasError, Cell};
use crate::color::Rgb;
use crate::palette::{HsbOffset, Palette, PaletteError};
use std::fmt;

/// The line that ends the art and begins the palette.
///
/// Searched for from the END of the file, so a drawing that happens to contain this exact line
/// is still read whole as long as it also carries a real trailer after it.
pub const PALETTE_MARKER: &str = "=== arterminal palette ===";

const RESET: &str = "\x1b[0m";

/// A drawing with its palette, as read from or about to be written to a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Document {
    pub canvas: Canvas,
    pub palette: Palette,
}

/// Read a document from the text of a file.
pub fn parse(text: &str) -> Result<Document, DocumentError> {
    let text = text.strip_suffix('\n').unwrap_or(text);
    let lines: Vec<&str> = text.split('\n').map(strip_carriage_return).collect();
    let marker = lines.iter().rposition(|line| *line == PALETTE_MARKER);
    let (body, trailer) = match marker {
        Some(at) => (&lines[..at], &lines[at + 1..]),
        None => (&lines[..], &[][..]),
    };

    let canvas = Canvas::from_rows(parse_body(body))?;
    let mut palette = parse_trailer(trailer, marker.map_or(0, |at| at + 2))?;

    // Every colour in the drawing gets a swatch, which is the rule that makes "recolour every
    // cell using this one" answerable. Walked in reading order so the counted names are stable.
    for cell in canvas.rows().flatten() {
        if let Some(ink) = cell.ink {
            if palette.holder_of(ink).is_none() {
                palette
                    .push_unnamed(ink)
                    .map_err(|source| DocumentError::Palette { line: 0, source })?;
            }
        }
    }
    Ok(Document { canvas, palette })
}

/// Write a document as the text of a file.
///
/// Rows are trimmed of trailing spaces before writing, whatever ink those spaces carried: a
/// foreground colour on a space draws nothing, so the ink was never visible, and keeping the
/// spaces would pad every row of a ragged drawing out to the widest — a diff on every line of a
/// file that was only opened.
pub fn render(canvas: &Canvas, palette: &Palette) -> String {
    let mut out = String::new();
    for row in canvas.rows() {
        let end = row.iter().rposition(|cell| cell.glyph != ' ').map_or(0, |at| at + 1);
        let mut ink = None;
        for cell in &row[..end] {
            if cell.ink != ink {
                out.push_str(&sgr(cell.ink));
                ink = cell.ink;
            }
            out.push(cell.glyph);
        }
        if ink.is_some() {
            out.push_str(RESET);
        }
        out.push('\n');
    }
    if !palette.is_empty() {
        out.push_str(PALETTE_MARKER);
        out.push('\n');
        for swatch in palette.iter() {
            out.push_str(swatch.label());
            out.push('\t');
            out.push_str(&swatch.color().to_string());
            if let Some(derivation) = swatch.derivation() {
                let offset = derivation.offset;
                out.push('\t');
                out.push_str(&derivation.base);
                out.push_str(&format!(
                    "\t{},{},{}",
                    offset.hue, offset.saturation, offset.brightness
                ));
            }
            out.push('\n');
        }
    }
    out
}

/// The escape that switches to `ink`, or back to the terminal's own colour for `None`.
fn sgr(ink: Option<Rgb>) -> String {
    match ink {
        Some(Rgb { r, g, b }) => format!("\x1b[38;2;{r};{g};{b}m"),
        None => RESET.to_string(),
    }
}

/// The art: text with escapes, into rows of cells carrying the colour in force.
fn parse_body(lines: &[&str]) -> Vec<Vec<Cell>> {
    let mut ink = None;
    lines
        .iter()
        .map(|line| {
            let mut row = Vec::new();
            for (segment, is_escape) in console::AnsiCodeIterator::new(line) {
                match is_escape {
                    true => ink = ink_after(segment, ink),
                    false => row.extend(segment.chars().map(|glyph| Cell { glyph, ink })),
                }
            }
            row
        })
        .collect()
}

/// The colour in force after `escape`, given `current` before it.
///
/// Reads only what this crate cares about — see the module docs — and leaves everything else
/// as it found it, so an unfamiliar sequence never darkens the rest of the file.
fn ink_after(escape: &str, current: Option<Rgb>) -> Option<Rgb> {
    let Some(params) = escape.strip_prefix("\x1b[").and_then(|rest| rest.strip_suffix('m')) else {
        return current; // not a colour sequence at all
    };
    if params.is_empty() {
        return None; // `ESC[m` is the short spelling of reset
    }
    let mut ink = current;
    let mut params = params.split(';').map(|p| p.parse::<u8>().ok());
    while let Some(param) = params.next() {
        match param {
            Some(0) | Some(39) => ink = None,
            Some(38) => {
                // Either `2;r;g;b` or `5;n`. Only the first names an RGB this crate can hold.
                match params.next() {
                    Some(Some(2)) => {
                        let (r, g, b) = (params.next(), params.next(), params.next());
                        if let (Some(Some(r)), Some(Some(g)), Some(Some(b))) = (r, g, b) {
                            ink = Some(Rgb::new(r, g, b));
                        }
                    }
                    Some(Some(5)) => {
                        params.next();
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
    ink
}

/// The palette lines, in the order they appear — with `first_line` the 1-based number of the
/// first of them, so an error can point at the file.
///
/// Plain swatches are added first and derived ones after, each once its base exists, because a
/// derivation can only be added to a base that is already there and the file may list them in
/// any order. That can leave a derived swatch displayed later than the file listed it; a picker
/// cannot yet make derived swatches, so the case is theoretical today, and noted here so it is
/// not a surprise when it stops being.
fn parse_trailer(lines: &[&str], first_line: usize) -> Result<Palette, DocumentError> {
    struct Entry<'a> {
        line: usize,
        label: &'a str,
        color: Rgb,
        derived: Option<(&'a str, HsbOffset)>,
    }
    let mut entries = Vec::new();
    for (offset, text) in lines.iter().enumerate() {
        let line = first_line + offset;
        if text.trim().is_empty() {
            continue;
        }
        let fields: Vec<&str> = text.split('\t').collect();
        let malformed = |why: &str| DocumentError::Trailer { line, why: why.to_string() };
        let (label, hex) =
            match fields.as_slice() {
                [label, hex] | [label, hex, _, _] => (*label, *hex),
                _ => return Err(malformed(
                    "expected `label<TAB>#rrggbb`, optionally followed by `<TAB>base<TAB>h,s,b`",
                )),
            };
        let color = Rgb::from_hex(hex).map_err(|err| malformed(&err.to_string()))?;
        let derived = match fields.as_slice() {
            [_, _, base, offset] => Some((
                *base,
                parse_offset(offset).ok_or_else(|| {
                    malformed(
                        "expected an offset like `0,0,-60` (hue steps, saturation, brightness)",
                    )
                })?,
            )),
            _ => None,
        };
        entries.push(Entry { line, label, color, derived });
    }

    let mut palette = Palette::new();
    let at = |line, source| DocumentError::Palette { line, source };
    for entry in entries.iter().filter(|e| e.derived.is_none()) {
        palette.push(entry.label, entry.color).map_err(|e| at(entry.line, e))?;
    }
    let mut pending: Vec<&Entry> = entries.iter().filter(|e| e.derived.is_some()).collect();
    while !pending.is_empty() {
        let before = pending.len();
        let mut still = Vec::new();
        for entry in pending {
            let (base, offset) = entry.derived.expect("filtered to derived");
            match palette.get(base) {
                Some(_) => palette
                    .push_derived(entry.label, base, offset)
                    .map_err(|e| at(entry.line, e))?,
                None => still.push(entry),
            }
        }
        if still.len() == before {
            let entry = still[0];
            return Err(at(
                entry.line,
                PaletteError::UnknownBase { label: entry.derived.expect("derived").0.to_string() },
            ));
        }
        pending = still;
    }
    Ok(palette)
}

/// `h,s,b` as three signed integers.
fn parse_offset(text: &str) -> Option<HsbOffset> {
    let mut parts = text.split(',').map(|part| part.trim().parse::<i32>().ok());
    let (hue, saturation, brightness) = (parts.next()??, parts.next()??, parts.next()??);
    parts.next().is_none().then_some(HsbOffset { hue, saturation, brightness })
}

fn strip_carriage_return(line: &str) -> &str {
    line.strip_suffix('\r').unwrap_or(line)
}

/// Why a file could not be read as a document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DocumentError {
    /// The art itself broke a canvas rule.
    Canvas(CanvasError),
    /// A palette line was not in the shape the trailer uses.
    Trailer { line: usize, why: String },
    /// A palette line was well-formed but the palette refused it — a duplicate, a missing base.
    /// `line` is 0 for a swatch the loader generated itself rather than read.
    Palette { line: usize, source: PaletteError },
}

impl From<CanvasError> for DocumentError {
    fn from(err: CanvasError) -> Self {
        Self::Canvas(err)
    }
}

impl fmt::Display for DocumentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Canvas(err) => err.fmt(f),
            Self::Trailer { line, why } => write!(f, "line {line}: {why}"),
            Self::Palette { line: 0, source } => write!(f, "palette: {source}"),
            Self::Palette { line, source } => write!(f, "line {line}: {source}"),
        }
    }
}

impl std::error::Error for DocumentError {}

#[cfg(test)]
mod tests {
    use super::*;

    const RED: Rgb = Rgb::new(226, 57, 57);
    const BLUE: Rgb = Rgb::new(57, 144, 226);

    fn inked(text: &str, ink: &[((usize, usize), Rgb)]) -> Canvas {
        let mut canvas = Canvas::from_text(text).expect("valid");
        for ((x, y), color) in ink {
            canvas.cell_mut(*x, *y).expect("in bounds").ink = Some(*color);
        }
        canvas
    }

    /// A drawing that was only looked at is saved as exactly the text it was — the format costs
    /// nothing until it is used.
    #[test]
    fn plain_text_survives_untouched() {
        let text = "  ..---..\n..--\"\"    \"-.\n\n#####\n";
        let doc = parse(text).expect("plain text is a document");
        assert!(doc.palette.is_empty());
        assert!(doc.canvas.rows().flatten().all(|cell| cell.ink.is_none()));
        assert_eq!(render(&doc.canvas, &doc.palette), text, "byte for byte");
    }

    #[test]
    fn trailing_spaces_are_trimmed_but_inner_ones_kept() {
        let doc = parse("ab   \n  c  \n").expect("valid");
        assert_eq!(render(&doc.canvas, &doc.palette), "ab\n  c\n");
    }

    #[test]
    fn colours_are_written_inline_and_the_palette_follows() {
        let canvas = inked("abc\ndef", &[((0, 0), RED), ((1, 0), RED), ((0, 1), BLUE)]);
        let mut palette = Palette::new();
        palette.push("ember", RED).unwrap();
        palette.push("sky", BLUE).unwrap();
        let text = render(&canvas, &palette);
        assert_eq!(
            text,
            "\x1b[38;2;226;57;57mab\x1b[0mc\n\
             \x1b[38;2;57;144;226md\x1b[0mef\n\
             === arterminal palette ===\n\
             ember\t#e23939\n\
             sky\t#3990e2\n"
        );
    }

    /// The contract of a save: what is written reads back as what was held.
    #[test]
    fn a_coloured_document_round_trips_exactly() {
        let canvas = inked("abc\ndef\nghi", &[((0, 0), RED), ((2, 1), BLUE), ((1, 2), RED)]);
        let mut palette = Palette::new();
        palette.push("ember", RED).unwrap();
        palette.push("sky", BLUE).unwrap();
        palette.push("unused", Rgb::new(1, 2, 3)).unwrap();
        let darker = HsbOffset { brightness: -60, ..HsbOffset::default() };
        palette.push_derived("shade", "ember", darker).unwrap();

        let text = render(&canvas, &palette);
        let back = parse(&text).expect("what we wrote is readable");
        assert_eq!(back.canvas, canvas);
        assert_eq!(back.palette, palette, "labels, colours, derivations and order all survive");
        assert_eq!(render(&back.canvas, &back.palette), text, "and a second save is identical");
    }

    /// A file with colours but no palette — made by hand, or by another tool — still loads, and
    /// every colour in it gets a swatch, because elsewhere the rule is that every colour has one.
    #[test]
    fn colours_with_no_palette_get_swatches_with_counted_names() {
        let text = "\x1b[38;2;226;57;57mab\x1b[0m\n\x1b[38;2;57;144;226mc\x1b[0m\n";
        let doc = parse(text).expect("valid");
        let labels: Vec<_> =
            doc.palette.iter().map(|s| (s.label().to_string(), s.color())).collect();
        assert_eq!(labels, [("colour 1".to_string(), RED), ("colour 2".to_string(), BLUE)]);
        assert_eq!(doc.canvas.cell(1, 0).unwrap().ink, Some(RED));
    }

    /// The marker is found from the end, so art that happens to contain it is still whole.
    #[test]
    fn art_may_contain_the_marker_line_if_a_real_trailer_follows() {
        let text = format!("{PALETTE_MARKER}\nart\n{PALETTE_MARKER}\nember\t#e23939\n");
        let doc = parse(&text).expect("valid");
        assert_eq!(doc.canvas.height(), 2, "the first marker line is art");
        assert_eq!(doc.palette.len(), 1);
    }

    /// Colour carries across line ends as it does on a terminal, so a hand-made file that sets
    /// a colour once reads the way it prints.
    #[test]
    fn colour_carries_over_a_line_end_until_reset() {
        let doc = parse("\x1b[38;2;226;57;57mab\ncd\x1b[0me\n").expect("valid");
        assert_eq!(doc.canvas.cell(0, 1).unwrap().ink, Some(RED), "still red on the next line");
        assert_eq!(doc.canvas.cell(2, 1).unwrap().ink, None, "until the reset");
    }

    /// Everything the loader is not interested in is passed over with its text kept.
    #[test]
    fn unfamiliar_escapes_are_skipped_and_their_text_kept() {
        let text = "\x1b[1mbold\x1b[0m \x1b[38;5;196mx\x1b[0m \x1b[48;2;1;2;3my\x1b[0m \x1b[39mz\n";
        let doc = parse(text).expect("valid");
        let glyphs: String = doc.canvas.row(0).unwrap().iter().map(|c| c.glyph).collect();
        assert_eq!(glyphs, "bold x y z");
        assert!(doc.canvas.rows().flatten().all(|c| c.ink.is_none()), "none of those set an ink");
        assert!(doc.palette.is_empty());
    }

    #[test]
    fn the_short_reset_and_default_foreground_both_clear() {
        let doc = parse("\x1b[38;2;226;57;57ma\x1b[mb\x1b[38;2;226;57;57mc\x1b[39md\n").unwrap();
        let inks: Vec<_> = doc.canvas.row(0).unwrap().iter().map(|c| c.ink).collect();
        assert_eq!(inks, [Some(RED), None, Some(RED), None]);
    }

    #[test]
    fn a_malformed_palette_line_names_its_line_number() {
        let text = "art\n=== arterminal palette ===\nember\t#e23939\nbroken line\n";
        let err = parse(text).expect_err("three fields is not a shape");
        assert!(matches!(err, DocumentError::Trailer { line: 4, .. }), "{err:?}");
        assert!(err.to_string().starts_with("line 4:"), "{err}");

        let bad_hex = "art\n=== arterminal palette ===\nember\tnot-a-colour\n";
        let err = parse(bad_hex).expect_err("not hex");
        assert!(matches!(err, DocumentError::Trailer { line: 3, .. }), "{err:?}");
    }

    #[test]
    fn a_palette_line_the_palette_refuses_names_its_line_number() {
        let text = "art\n=== arterminal palette ===\nember\t#e23939\nflame\t#e23939\n";
        let err = parse(text).expect_err("duplicate colour");
        assert!(
            matches!(
                err,
                DocumentError::Palette { line: 4, source: PaletteError::DuplicateColor { .. } }
            ),
            "{err:?}"
        );
    }

    /// A derived swatch may be listed before its base; the loader sorts that out. One with no
    /// base anywhere is an error at its own line.
    #[test]
    fn derived_swatches_may_precede_their_base_but_not_lack_one() {
        let text =
            "art\n=== arterminal palette ===\nshade\t#000000\tember\t0,0,-60\nember\t#e23939\n";
        let doc = parse(text).expect("base comes later, that is fine");
        assert_eq!(doc.palette.get("shade").unwrap().derivation().unwrap().base, "ember");

        let orphan = "art\n=== arterminal palette ===\nshade\t#000000\tnobody\t0,0,-60\n";
        let err = parse(orphan).expect_err("no such base");
        assert!(
            matches!(
                err,
                DocumentError::Palette { line: 3, source: PaletteError::UnknownBase { .. } }
            ),
            "{err:?}"
        );
    }

    #[test]
    fn a_canvas_rule_broken_in_the_art_is_reported_as_such() {
        let err = parse("ok\nbad\there\n").expect_err("tabs are refused");
        assert!(matches!(err, DocumentError::Canvas(CanvasError::Control { .. })), "{err:?}");
    }

    #[test]
    fn windows_line_ends_are_accepted_in_body_and_trailer() {
        let text = "ab\r\n=== arterminal palette ===\r\nember\t#e23939\r\n";
        let doc = parse(text).expect("valid");
        assert_eq!(doc.canvas.width(), 2);
        assert_eq!(doc.palette.get("ember").map(|s| s.color()), Some(RED));
    }
}
