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
//! More than [`render`] writes, on purpose — a file may have been made by hand, or by another
//! tool. Escape sequences are cut out by ECMA-48's own grammar, the terminal's, so what counts as
//! text here is exactly what a terminal would draw; see `pieces`.
//!
//! The 24-bit foreground sets a colour, in either spelling: `ESC[38;2;r;g;b m`, which is what
//! [`render`] writes, and the ITU T.416 form `ESC[38:2::r:g:b m` (also without its empty
//! colour-space field). The 16- and 256-colour foregrounds — `ESC[31m`, `ESC[91m`,
//! `ESC[38;5;n m`, `ESC[38:5:n m` — set a terminal palette SLOT, kept as one (see [`Ink`]), and
//! [`render`] writes every slot back as `ESC[38;5;n m`: the spelling the picker itself draws
//! them in, so what is saved is what was seen. `ESC[0m` and `ESC[39m` clear the colour.
//! Everything else, bold and backgrounds and underline colours included, is passed over; the
//! arguments of a background or underline colour are CONSUMED with it, so `ESC[48;2;31;0;0m`
//! never reads its red channel as "foreground red". Colour CARRIES across line ends, as it does
//! on a terminal, so a file that sets a colour once and never resets it reads the way it prints.
//! Rows [`render`] writes always reset at their end, so nothing this crate saved depends on that.

use crate::canvas::{Canvas, CanvasError, Cell};
use crate::color::{Ink, Rgb};
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
    let lines = lines(text);
    let (body, trailer, first_trailer_line) = split(&lines);
    let canvas = Canvas::from_rows(parse_body(body))?;
    let mut palette = parse_trailer(trailer, first_trailer_line)?;

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

/// The text of the file at `path`, as every loader here reads it — see [`Picker::open`] for the
/// usual way in. Public for a caller that wants to [`measure`] a file before it builds anything.
///
/// [`Picker::open`]: crate::Picker::open
pub fn read(path: impl AsRef<std::path::Path>) -> Result<String, crate::LoadError> {
    let path = path.as_ref();
    crate::canvas::read_text(path)
        .map_err(|source| crate::LoadError { path: path.to_path_buf(), source })
}

/// How big a document's drawing is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Size {
    /// In cells: the widest row, since every row is padded out to it.
    pub width: usize,
    pub height: usize,
}

impl Size {
    /// Past this many cells in either direction, the binary asks before opening.
    pub const ASK_ABOVE: usize = 10_000;

    /// Past this many cells in all, it asks too: the product is what predicts memory, and a
    /// 9000 × 9000 drawing is within both sides and the biggest of all. The developer's rule,
    /// both limits, and this is where it lives.
    pub const ASK_ABOVE_CELLS: usize = 10_000_000;

    /// Every cell of the drawing, rows padded out to the widest.
    pub fn cells(self) -> usize {
        self.width.saturating_mul(self.height)
    }

    /// Whether opening this should be asked about first — see [`Size::ASK_ABOVE`] and
    /// [`Size::ASK_ABOVE_CELLS`].
    pub fn is_large(self) -> bool {
        self.width > Self::ASK_ABOVE
            || self.height > Self::ASK_ABOVE
            || self.cells() > Self::ASK_ABOVE_CELLS
    }

    /// Bytes the canvas takes once built: a cell for every column of every row. What the rest of
    /// a session adds — the undo history, a frame — comes on top.
    pub fn canvas_bytes(self) -> usize {
        self.cells().saturating_mul(std::mem::size_of::<Cell>())
    }
}

/// How big the drawing in `text` is, found WITHOUT building it — which is the point: a single
/// long line pads every other row out to its width, so a small file can need a great deal of
/// memory, and this is how to find out before paying for it.
pub fn measure(text: &str) -> Size {
    let lines = lines(text);
    let (body, _, _) = split(&lines);
    let width = body
        .iter()
        .map(|line| {
            pieces(line)
                .iter()
                .map(|piece| match piece {
                    Piece::Text(text) => text.chars().count(),
                    _ => 0,
                })
                .sum::<usize>()
        })
        .max()
        .unwrap_or(0);
    Size { width, height: body.len() }
}

/// The lines of a file's text: a byte-order mark and one trailing newline taken off, and each
/// line's carriage return, so a file written on Windows reads like any other.
fn lines(text: &str) -> Vec<&str> {
    // A UTF-8 byte-order mark is an encoding's signature, not a glyph of the drawing.
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let text = text.strip_suffix('\n').unwrap_or(text);
    text.split('\n').map(strip_carriage_return).collect()
}

/// The art, the palette lines after the LAST marker, and the 1-based line number the palette
/// starts on.
fn split<'a, 'b>(lines: &'b [&'a str]) -> (&'b [&'a str], &'b [&'a str], usize) {
    match lines.iter().rposition(|line| *line == PALETTE_MARKER) {
        Some(at) => (&lines[..at], &lines[at + 1..], at + 2),
        None => (lines, &[], 0),
    }
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
///
/// Every slot as `38;5;n`, the first sixteen included, although `31` or `91` would name them in
/// fewer bytes: `38;5;n` is how the picker DRAWS a slot, and some terminals give the two spellings
/// of one of the first sixteen different colours — so this is the spelling in which what was
/// saved is what was seen.
fn sgr(ink: Option<Ink>) -> String {
    match ink {
        Some(Ink::Rgb(Rgb { r, g, b })) => format!("\x1b[38;2;{r};{g};{b}m"),
        Some(Ink::Slot(slot)) => format!("\x1b[38;5;{slot}m"),
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
            for piece in pieces(line) {
                match piece {
                    Piece::Text(text) => row.extend(text.chars().map(|glyph| Cell { glyph, ink })),
                    Piece::Csi { params, intermediates: "", final_byte: b'm' } => {
                        ink = ink_after(params, ink);
                    }
                    Piece::Csi { .. } | Piece::Other => {}
                }
            }
            row
        })
        .collect()
}

/// One piece of a line of art, as a terminal would read it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Piece<'a> {
    /// Text to draw.
    Text(&'a str),
    /// A control sequence: `ESC [` — or the one-character C1 `CSI` — then its parameter bytes,
    /// its intermediate bytes and its final byte.
    Csi { params: &'a str, intermediates: &'a str, final_byte: u8 },
    /// Any other escape, consumed whole and meaning nothing here: an OSC or DCS string, a
    /// character-set switch, a sequence the end of the line cut off.
    Other,
}

/// `line`, cut into [`Piece`]s by ECMA-48's grammar — the terminal's own reading of it.
///
/// A CSI is its introducer, parameter bytes (0x30-0x3F, which INCLUDES `:`), intermediate bytes
/// (0x20-0x2F) and a final byte (0x40-0x7E). An OSC runs to BEL or ST; a DCS, SOS, PM or APC
/// string runs to ST. Any other ESC takes its intermediates and one final byte (0x30-0x7E). A
/// sequence cut off by the end of the line, or by a byte it cannot hold, is dropped as far as
/// it got. An ESC followed by nothing a sequence can start with is left in the text — where the
/// canvas refuses it by line and column, rather than this guessing what was meant.
///
/// Written here rather than taken from `console`, whose `AnsiCodeIterator` ended a CSI at its
/// first `:` — so the ITU spelling of a 24-bit colour, `ESC[38:2::255:0:0m`, loaded as the text
/// `:2::255:0:0m` in the drawing, and saving wrote that back into the file.
fn pieces(line: &str) -> Vec<Piece<'_>> {
    let bytes = line.as_bytes();
    let within = |at: usize, range: std::ops::RangeInclusive<u8>| {
        at + bytes[at..].iter().take_while(|b| range.contains(b)).count()
    };
    let mut pieces = Vec::new();
    let (mut text, mut at) = (0, 0);
    while at < bytes.len() {
        // ESC and the lead byte of U+009B are ASCII and a UTF-8 lead byte respectively, so every
        // cut made here falls on a character boundary.
        let (piece, next) = match (bytes[at], bytes.get(at + 1)) {
            (0x1b, Some(b'[')) | (0xc2, Some(0x9b)) => {
                let params = within(at + 2, 0x30..=0x3f);
                let intermediates = within(params, 0x20..=0x2f);
                match bytes.get(intermediates) {
                    Some(&final_byte) if (0x40..=0x7e).contains(&final_byte) => (
                        Piece::Csi {
                            params: &line[at + 2..params],
                            intermediates: &line[params..intermediates],
                            final_byte,
                        },
                        intermediates + 1,
                    ),
                    _ => (Piece::Other, intermediates),
                }
            }
            (0x1b, Some(&kind @ (b']' | b'P' | b'X' | b'^' | b'_'))) => {
                let rest = &bytes[at + 2..];
                let end = (0..rest.len()).find_map(|i| match (rest[i], rest.get(i + 1)) {
                    (0x07, _) if kind == b']' => Some(i + 1),
                    (0x1b, Some(b'\\')) | (0xc2, Some(0x9c)) => Some(i + 2),
                    _ => None,
                });
                (Piece::Other, end.map_or(bytes.len(), |end| at + 2 + end))
            }
            (0x1b, Some(0x20..=0x7e)) => {
                let intermediates = within(at + 1, 0x20..=0x2f);
                match bytes.get(intermediates) {
                    Some(0x30..=0x7e) => (Piece::Other, intermediates + 1),
                    _ => (Piece::Other, intermediates),
                }
            }
            _ => {
                at += 1;
                continue;
            }
        };
        if text < at {
            pieces.push(Piece::Text(&line[text..at]));
        }
        pieces.push(piece);
        (text, at) = (next, next);
    }
    if text < bytes.len() {
        pieces.push(Piece::Text(&line[text..]));
    }
    pieces
}

/// The foreground in force after an SGR with `params`, given `current` before it.
///
/// Reads the foreground and nothing else, in both spellings: parameters split on `;`, and each
/// may carry `:`-separated sub-parameters. A colour's arguments are consumed with it whichever it
/// is for, so a background's channels are never read as codes of their own. A private SGR — xterm
/// uses `ESC[>4;2m` for a keyboard mode — is not a colour at all.
fn ink_after(params: &str, current: Option<Ink>) -> Option<Ink> {
    if params.starts_with(['<', '=', '>', '?']) {
        return current;
    }
    if params.is_empty() {
        return None; // `ESC[m` is the short spelling of reset
    }
    let mut ink = current;
    let mut list = params.split(';');
    while let Some(param) = list.next() {
        let mut parts = param.split(':');
        let code = parts.next().map_or(Some(0), number);
        let sub: Vec<&str> = parts.collect();
        match code {
            Some(0 | 39) => ink = None,
            Some(code @ 30..=37) => ink = Some(Ink::Slot((code - 30) as u8)),
            Some(code @ 90..=97) => ink = Some(Ink::Slot((code - 90 + 8) as u8)),
            Some(code @ (38 | 48 | 58)) => {
                let colour = match sub.is_empty() {
                    true => spelled_with_semicolons(&mut list),
                    false => spelled_with_colons(&sub),
                };
                if let (38, Some(colour)) = (code, colour) {
                    ink = Some(colour);
                }
            }
            _ => {}
        }
    }
    ink
}

/// The rest of `38;2;r;g;b` or `38;5;n`, from the parameters that follow the `38`.
fn spelled_with_semicolons<'a>(list: &mut impl Iterator<Item = &'a str>) -> Option<Ink> {
    match number(list.next()?)? {
        2 => {
            let (r, g, b) = (list.next()?, list.next()?, list.next()?);
            Some(Ink::Rgb(Rgb::new(channel(r)?, channel(g)?, channel(b)?)))
        }
        5 => Some(Ink::Slot(channel(list.next()?)?)),
        _ => None,
    }
}

/// The rest of `38:2::r:g:b`, `38:2:r:g:b` or `38:5:n`, from the sub-parameters after the `38`.
fn spelled_with_colons(sub: &[&str]) -> Option<Ink> {
    match (number(sub.first()?)?, sub.len()) {
        // With the colour-space field — empty or not — the channels come after it.
        (2, 5..) => Some(Ink::Rgb(Rgb::new(channel(sub[2])?, channel(sub[3])?, channel(sub[4])?))),
        (2, 4) => Some(Ink::Rgb(Rgb::new(channel(sub[1])?, channel(sub[2])?, channel(sub[3])?))),
        (5, 2..) => Some(Ink::Slot(channel(sub[1])?)),
        _ => None,
    }
}

/// A parameter's number; empty is the default, 0, as ECMA-48 has it.
fn number(text: &str) -> Option<u32> {
    match text {
        "" => Some(0),
        text => text.parse().ok(),
    }
}

/// A colour channel or palette slot: a number that fits in a byte.
fn channel(text: &str) -> Option<u8> {
    number(text).and_then(|n| u8::try_from(n).ok())
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
        color: Ink,
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
                    "expected `label<TAB>#rrggbb` or `label<TAB>slot n`, optionally followed by \
                     `<TAB>base<TAB>h,s,b`",
                )),
            };
        let color = Ink::parse(hex).map_err(|err| malformed(&err.to_string()))?;
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
            canvas.cell_mut(*x, *y).expect("in bounds").ink = Some((*color).into());
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
        assert_eq!(
            labels,
            [("colour 1".to_string(), RED.into()), ("colour 2".to_string(), BLUE.into())]
        );
        assert_eq!(doc.canvas.cell(1, 0).unwrap().ink, Some(RED.into()));
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
        assert_eq!(
            doc.canvas.cell(0, 1).unwrap().ink,
            Some(RED.into()),
            "still red on the next line"
        );
        assert_eq!(doc.canvas.cell(2, 1).unwrap().ink, None, "until the reset");
    }

    /// Everything the loader is not interested in is passed over with its text kept.
    #[test]
    fn unfamiliar_escapes_are_skipped_and_their_text_kept() {
        // (A 256-colour code is not among them: it names a terminal palette slot, which loads as
        // one — see `palette_slot_colours_load_as_slots_with_swatches`.)
        let text = "\x1b[1mbold\x1b[0m \x1b[4mx\x1b[0m \x1b[48;2;1;2;3my\x1b[0m \x1b[39mz\n";
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
        assert_eq!(inks, [Some(RED.into()), None, Some(RED.into()), None]);
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
        assert_eq!(doc.palette.get("ember").map(|s| s.color()), Some(RED.into()));
    }

    // ---- escapes as a terminal reads them ------------------------------------------------------

    fn glyphs(doc: &Document, y: usize) -> String {
        doc.canvas.row(y).expect("row").iter().map(|cell| cell.glyph).collect()
    }

    fn inks(doc: &Document, y: usize) -> Vec<Option<Ink>> {
        doc.canvas.row(y).expect("row").iter().map(|cell| cell.ink).collect()
    }

    /// The ITU spelling of a 24-bit colour, with its colour-space field and without, is a colour
    /// like the semicolon one — and none of the escape is left behind as text. (It used to load
    /// as the glyphs `:2::255:0:0m`, which saving then wrote into the file.)
    #[test]
    fn a_colour_spelled_with_colons_is_a_colour_and_leaves_no_text() {
        let red = Rgb::new(255, 0, 0);
        for text in ["\x1b[38:2::255:0:0mab\x1b[0m", "\x1b[38:2:255:0:0mab\x1b[0m"] {
            let doc = parse(text).expect("loads");
            assert_eq!(glyphs(&doc, 0), "ab", "{text:?}");
            assert_eq!(inks(&doc, 0), [Some(red.into()); 2], "{text:?}");
        }
    }

    /// Everything this crate writes, it reads back exactly: its own spelling is the one real
    /// files use most, so it is the one a parser change is most likely to break.
    #[test]
    fn what_render_writes_parse_reads_back_whole() {
        let (red, blue) = (Rgb::new(226, 57, 57), Rgb::new(57, 144, 226));
        let canvas =
            inked("abcd\nefgh", &[((0, 0), red), ((1, 0), red), ((3, 0), blue), ((2, 1), blue)]);
        let mut palette = Palette::new();
        palette.push("red", red).expect("fresh");
        palette.push("blue", blue).expect("fresh");
        let text = render(&canvas, &palette);
        assert_eq!(parse(&text).expect("loads"), Document { canvas, palette });
    }

    /// 16- and 256-colour codes, in every spelling, load as terminal palette SLOTS — kept as
    /// slots, not guessed at — and each slot gets a swatch like any colour found without one.
    #[test]
    fn palette_slot_colours_load_as_slots_with_swatches() {
        let doc =
            parse("\x1b[31mab\x1b[91mc\x1b[38;5;196md\x1b[38:5:21me\nx\x1b[0my\n").expect("loads");
        let slot = |n| Some(Ink::Slot(n));
        assert_eq!(inks(&doc, 0), [slot(1), slot(1), slot(9), slot(196), slot(21)]);
        assert_eq!(inks(&doc, 1)[..2], [slot(21), None], "carried over the line end, then reset");
        let held: Vec<Ink> = doc.palette.iter().map(|swatch| swatch.color()).collect();
        assert_eq!(held, [Ink::Slot(1), Ink::Slot(9), Ink::Slot(196), Ink::Slot(21)]);
    }

    /// What a file said, it says again: every spelling of a slot is read as the slot it names,
    /// written back as `38;5;n` — the spelling the picker draws in — and read back as the same
    /// slot, so a load and a save lose nothing.
    #[test]
    fn a_slot_in_any_spelling_survives_a_load_and_a_save() {
        let first =
            parse("\x1b[31ma\x1b[91mb\x1b[38;5;200mc\x1b[38:5:16md\x1b[0m\n").expect("loads");
        let written = render(&first.canvas, &first.palette);
        assert!(
            written.starts_with("\x1b[38;5;1ma\x1b[38;5;9mb\x1b[38;5;200mc\x1b[38;5;16md"),
            "{written:?}"
        );
        let second = parse(&written).expect("reloads");
        assert_eq!(second, first, "the same slots, the same palette");
    }

    /// A slot swatch is written into the palette as `slot n`, and read back as one; a slot past
    /// 255 is refused with the line it is on, never wrapped or coerced into something it is not.
    #[test]
    fn slot_swatches_are_written_and_read_as_slot_n() {
        let mut palette = Palette::new();
        palette.push("sea", Ink::Slot(24)).expect("fresh");
        palette.push("ember", RED).expect("fresh");
        let text = render(&Canvas::from_text("ab").unwrap(), &palette);
        assert!(text.contains("sea\tslot 24\n") && text.contains("ember\t#e23939\n"), "{text:?}");
        assert_eq!(parse(&text).expect("reloads").palette, palette);

        let refused =
            parse("ab\n=== arterminal palette ===\nsea\tslot 256\n").expect_err("no slot 256");
        assert!(refused.to_string().contains("line 3"), "{refused}");
    }

    /// A swatch cannot follow a slot: what a slot looks like is the terminal's to say, and a
    /// derivation would freeze one terminal's reading of it into the file.
    #[test]
    fn a_swatch_cannot_follow_a_slot() {
        let text = "ab\n=== arterminal palette ===\nsea\tslot 24\nshade\t#000000\tsea\t0,0,-10\n";
        let refused = parse(text).expect_err("refused").to_string();
        assert!(refused.contains("slot"), "{refused}");
    }

    /// A background's or an underline's colour is consumed with it, in both spellings, so its
    /// channels are never read as codes of their own — here, a red channel of 31 is not
    /// "foreground red".
    #[test]
    fn background_and_underline_colours_are_consumed_whole() {
        for text in
            ["\x1b[48;2;31;0;0mab", "\x1b[48;5;31mab", "\x1b[58:2::31:0:0mab", "\x1b[58;5;31mab"]
        {
            let doc = parse(text).expect("loads");
            assert_eq!(
                inks(&doc, 0),
                [None, None],
                "{text:?} took a background channel for a code"
            );
            assert_eq!(glyphs(&doc, 0), "ab");
        }
        let doc = parse("\x1b[48;2;1;2;3;31mab").expect("loads");
        assert_eq!(
            inks(&doc, 0),
            [Some(Ink::Slot(1)); 2],
            "a code AFTER the background's arguments"
        );
    }

    /// A private SGR is not a colour — xterm's `ESC[>4;2m` sets a keyboard mode — and a control
    /// sequence with intermediate bytes is consumed whole, whatever it is for.
    #[test]
    fn private_and_intermediate_sequences_are_passed_over_whole() {
        let doc = parse("\x1b[>4;2ma\x1b[?1$pb\x1b[2 qc").expect("loads");
        assert_eq!(glyphs(&doc, 0), "abc");
        assert!(doc.canvas.rows().flatten().all(|c| c.ink.is_none()), "nothing took a colour");
    }

    /// Strings run to their terminator — an OSC to BEL or ST, a DCS to ST — and one never closed
    /// takes the rest of its line with it; the one-character C1 CSI is a CSI like `ESC [`.
    #[test]
    fn strings_run_to_their_terminator_and_the_c1_csi_is_a_csi() {
        let doc = parse("a\x1b]0;title\x07b\x1b]8;;https://x\x1b\\c\x1bPq#0\x1b\\d\u{9b}1me")
            .expect("loads");
        assert_eq!(glyphs(&doc, 0), "abcde");
        let cut = parse("ab\x1b]0;never closed\ncd").expect("loads");
        assert_eq!((glyphs(&cut, 0), glyphs(&cut, 1)), ("ab".to_string(), "cd".to_string()));
    }

    /// An ESC followed by nothing a sequence can start with stays in the text, where the canvas
    /// refuses it by line and column rather than the loader guessing what was meant.
    #[test]
    fn an_escape_that_starts_nothing_is_refused_where_it_stands() {
        let refused = parse("ab\x1b\n").expect_err("refused").to_string();
        assert!(refused.contains("line 1, column 3"), "{refused}");
    }

    /// A document's size is found without building it: the widest row in glyphs — escapes take
    /// no room — by the art's lines, the palette's not among them.
    #[test]
    fn a_document_is_measured_without_being_built() {
        let text =
            "\u{feff}\x1b[38;2;1;2;3mabc\x1b[0m\nde\n=== arterminal palette ===\nred\t#ff0000\n";
        assert_eq!(measure(text), Size { width: 3, height: 2 });
        let long = format!("{}\n{}", "x".repeat(5_000), "a\n".repeat(999));
        let size = measure(&long);
        assert_eq!(size, Size { width: 5_000, height: 1_000 });
        assert_eq!(size.canvas_bytes(), 5_000 * 1_000 * std::mem::size_of::<Cell>());
    }

    /// Past the developer's lines — either side, or the whole — and only past them, opening asks
    /// first.
    #[test]
    fn only_a_drawing_past_a_line_is_large() {
        let (side, cells) = (Size::ASK_ABOVE, Size::ASK_ABOVE_CELLS);
        assert!(Size { width: side + 1, height: 1 }.is_large(), "too wide");
        assert!(Size { width: 1, height: side + 1 }.is_large(), "too tall");
        assert!(
            Size { width: 9_000, height: 9_000 }.is_large(),
            "within both sides, too many cells"
        );
        assert!(!Size { width: side, height: cells / side }.is_large(), "exactly on both lines");
        assert!(
            Size { width: side, height: cells / side + 1 }.is_large(),
            "one row past the whole"
        );
        assert!(!Size { width: 20, height: 3 }.is_large());
    }

    /// A byte-order mark leading the text is an encoding's signature, not part of the drawing.
    #[test]
    fn a_leading_byte_order_mark_is_not_part_of_the_drawing() {
        let doc = parse("\u{feff}ab\ncd\n").expect("loads");
        assert_eq!((glyphs(&doc, 0), glyphs(&doc, 1)), ("ab".to_string(), "cd".to_string()));
    }
}
