//! Version 1 of the crumpled form: a drawing's document as two planes — its characters, Huffman
//! coded, a row to a line, and its colours, a row's runs of palette numbers to a line — with the
//! palette itself as the document has it.
//!
//! ```text
//! === arterminal crumpled 1 ===
//! DCgBDhBC...         the table: how long each token's code is, and which characters there are
//! k3E_d8aQ            a row of characters, as base64 bits
//!                     a row with none
//! === colours ===     only if anything is coloured: a line for each row, empty if it has none
//! BdCE
//!
//! === palette ===     only if there is a palette: its lines exactly as the document has them
//! ember<TAB>#e23939
//! ```
//!
//! # The two planes
//!
//! A row's characters are TOKENS: a character, or RUN followed by a number — that many and one
//! more of the character before — or END, which closes the row. A run of three or more of one
//! character is its first, then RUN. The tokens are Huffman coded with one table for the whole
//! drawing, since a drawing uses few characters and a few of them most — a plain poster is half
//! spaces — and the table is the one thing written about them that is not a row.
//!
//! A row's colours are pairs: which palette entry, counting from 1 with 0 for none, and for how
//! many characters. Where the row's colour ends, the rest of it is uncoloured, and a row with no
//! colour at all is an empty line. The numbers are the palette's ORDER, which this crate only
//! ever adds to at the end — there is no edit that takes a swatch out of the middle or moves one
//! — so a number keeps its meaning, and painting one row changes one line. An edit that removed
//! or reordered swatches would renumber every colour line after it, and would need that said.
//!
//! # What changes, when the drawing does
//!
//! Painting a row changes its colour line; a swatch recoloured, renamed or added, its palette
//! line. A character swapped for another the drawing already has as often changes its row's line
//! alone. A character added or taken away changes how often each is used, can change the table,
//! and with it every row — but the picker never changes a character, only colours, so that
//! happens when the art itself is replaced, when all of it changes anyway.
//!
//! # One writer
//!
//! A document is written this way only if it is EXACTLY as [`document::render`] writes it, within
//! the size a drawing opens without asking — see [`encode`] — and decoded, it is rebuilt as a
//! drawing and written by that same function. So there is one writer of documents rather than
//! two, and the bytes a crumpled form decodes to are the ones the writer spells: change the
//! writer's spelling, and version 1's meaning changes with it. The tripwire in
//! `crate::crumple`'s tests fails first.

use super::base64::{self, Reader, Varints, Writer};
use super::huffman::{self, Decoder};
use super::UncrumpleError;
use crate::canvas::{Canvas, Cell};
use crate::color::Ink;
use crate::document::{self, Size, PALETTE_MARKER};
use std::collections::HashMap;

/// The marker line before the colour plane.
const COLOURS: &str = "=== colours ===";

/// The marker line before the palette.
const PALETTE: &str = "=== palette ===";

/// The token numbers: RUN, then END, then each character of the drawing in the order of its
/// code point.
const RUN: usize = 0;
const END: usize = 1;
const FIRST_GLYPH: usize = 2;

/// The longest run length a gamma code may say, in bits: past any row a crumpled form can hold,
/// which is at most [`Size::ASK_ABOVE`] characters.
const RUN_BITS: u32 = 32;

/// One token of a row: a character, by its number in the table; or a run, of this many and one
/// more of the character before; or the end of the row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Token {
    Glyph(usize),
    Run(u64),
    End,
}

impl Token {
    fn symbol(self) -> usize {
        match self {
            Token::Glyph(at) => FIRST_GLYPH + at,
            Token::Run(_) => RUN,
            Token::End => END,
        }
    }
}

/// `text` in version 1, everything after the header — or `None` when it cannot be: it is not a
/// document exactly as the writer writes it, it is past the size a drawing opens without asking,
/// or its characters would need a code longer than any reader allows.
///
/// The size line bounds what version 1 can hold, so a reader can refuse anything larger without
/// building it: a crumpled form that claims more is damaged or made to do harm.
pub(super) fn encode(text: &str) -> Option<String> {
    if document::measure(text).is_large() {
        return None;
    }
    let doc = document::parse(text).ok()?;
    if document::render(&doc.canvas, &doc.palette) != text {
        return None;
    }
    let rows: Vec<&[Cell]> = document::written_rows(&doc.canvas);

    let mut alphabet: Vec<char> = rows.iter().flat_map(|row| row.iter().map(|c| c.glyph)).collect();
    alphabet.sort_unstable();
    alphabet.dedup();
    let number: HashMap<char, usize> =
        alphabet.iter().enumerate().map(|(at, g)| (*g, at)).collect();
    let tokens: Vec<Vec<Token>> = rows.iter().map(|row| tokenise(row, &number)).collect();
    let mut counts = vec![0u64; FIRST_GLYPH + alphabet.len()];
    for token in tokens.iter().flatten() {
        counts[token.symbol()] += 1;
    }
    let lengths = huffman::lengths(&counts)?;
    let codes = huffman::codes(&lengths);

    let mut out = table(&lengths, &alphabet);
    out.push('\n');
    for row in &tokens {
        out.push_str(&glyph_line(row, &codes, &lengths));
        out.push('\n');
    }
    if rows.iter().flat_map(|row| row.iter()).any(|cell| cell.ink.is_some()) {
        let numbered: HashMap<Ink, usize> =
            doc.palette.iter().enumerate().map(|(at, swatch)| (swatch.color(), at + 1)).collect();
        out.push_str(COLOURS);
        out.push('\n');
        for row in &rows {
            out.push_str(&colour_line(row, &numbered));
            out.push('\n');
        }
    }
    if !doc.palette.is_empty() {
        // The document's own palette lines, after its marker: every line past the rows but the
        // empty one its last newline leaves.
        let lines: Vec<&str> = text.split('\n').collect();
        out.push_str(PALETTE);
        out.push('\n');
        for line in &lines[rows.len() + 1..lines.len() - 1] {
            out.push_str(line);
            out.push('\n');
        }
    }
    Some(out)
}

/// A row's tokens: each character, a run of three or more of one as its first and a RUN, and the
/// END. None at all for an empty row, which is an empty line.
fn tokenise(row: &[Cell], number: &HashMap<char, usize>) -> Vec<Token> {
    let mut tokens = Vec::new();
    let mut x = 0;
    while x < row.len() {
        let glyph = row[x].glyph;
        let run = row[x..].iter().take_while(|cell| cell.glyph == glyph).count();
        tokens.push(Token::Glyph(number[&glyph]));
        match run {
            1 => {}
            2 => tokens.push(Token::Glyph(number[&glyph])),
            _ => tokens.push(Token::Run(run as u64 - 2)),
        }
        x += run;
    }
    if !tokens.is_empty() {
        tokens.push(Token::End);
    }
    tokens
}

/// The table line: RUN's code length and END's, as a base64 digit each, then for each character,
/// how far its code point is past the one before as a varint, and its code length. Far places
/// are one character for most drawings' characters, which sit close together — a braille
/// drawing's all in one block of 256 — so a character costs two bytes here, not its own three.
fn table(lengths: &[u32], alphabet: &[char]) -> String {
    let mut out = String::new();
    out.push(base64::digit(lengths[RUN]));
    out.push(base64::digit(lengths[END]));
    let mut before = 0;
    for (at, glyph) in alphabet.iter().enumerate() {
        base64::push_varint(&mut out, (*glyph as u64) - before);
        out.push(base64::digit(lengths[FIRST_GLYPH + at]));
        before = *glyph as u64;
    }
    out
}

/// A row's tokens in their codes, as base64.
fn glyph_line(tokens: &[Token], codes: &[u64], lengths: &[u32]) -> String {
    let mut bits = Writer::default();
    for &token in tokens {
        let symbol = token.symbol();
        bits.bits(codes[symbol], lengths[symbol]);
        if let Token::Run(more) = token {
            bits.gamma(more);
        }
    }
    bits.finish()
}

/// A row's colour runs as varints: the palette number and the length of each, the uncoloured
/// run the row may end on left off.
fn colour_line(row: &[Cell], numbered: &HashMap<Ink, usize>) -> String {
    let mut out = String::new();
    let mut runs = Vec::new();
    let mut x = 0;
    while x < row.len() {
        let ink = row[x].ink;
        let run = row[x..].iter().take_while(|cell| cell.ink == ink).count();
        runs.push((ink.map_or(0, |ink| numbered[&ink]), run));
        x += run;
    }
    if runs.last().is_some_and(|&(number, _)| number == 0) {
        runs.pop();
    }
    for (number, run) in runs {
        base64::push_varint(&mut out, number as u64);
        base64::push_varint(&mut out, run as u64);
    }
    out
}

/// Version 1's `body` — everything after the header, which is line 1 — as the document it
/// stands for. Strict, and bounded: this is text from anywhere, and anything a writer of version
/// 1 would not have written is refused by line, never guessed at, and never allowed to build a
/// drawing past the size line.
pub(super) fn decode(body: &str) -> Result<String, UncrumpleError> {
    let mut lines: Vec<&str> =
        body.split('\n').map(|line| line.strip_suffix('\r').unwrap_or(line)).collect();
    if lines.last() == Some(&"") {
        lines.pop();
    }
    // Line numbers as a person counts them: the header is 1, the table 2.
    let malformed = |at: usize, why| UncrumpleError::Malformed { line: at + 2, why };
    let (table, rest) = lines.split_first().ok_or(malformed(0, "no table line"))?;
    let (decoder, alphabet) = read_table(table).ok_or(malformed(0, "not a table of codes"))?;

    let glyph_lines = rest.iter().take_while(|line| **line != COLOURS && **line != PALETTE).count();
    if glyph_lines == 0 || glyph_lines > Size::ASK_ABOVE {
        return Err(malformed(1, "more rows, or fewer, than any drawing crumpled so"));
    }
    let mut rows: Vec<Vec<char>> = Vec::with_capacity(glyph_lines);
    for (at, line) in rest[..glyph_lines].iter().enumerate() {
        rows.push(read_glyphs(line, &decoder, &alphabet).map_err(|why| malformed(at + 1, why))?);
    }
    let widest = rows.iter().map(Vec::len).max().unwrap_or(0);
    if widest * rows.len() > Size::ASK_ABOVE_CELLS {
        return Err(malformed(1, "a drawing larger than any crumpled so"));
    }

    let mut next = 1 + glyph_lines;
    let mut colour_lines: &[&str] = &[];
    if lines.get(next) == Some(&COLOURS) {
        colour_lines = lines
            .get(next + 1..next + 1 + rows.len())
            .ok_or(malformed(next, "fewer colour lines than rows"))?;
        next += 1 + rows.len();
    }
    let palette_lines: &[&str] = match lines.get(next) {
        None => &[],
        Some(&PALETTE) if lines.len() > next + 1 => &lines[next + 1..],
        Some(&PALETTE) => return Err(malformed(next, "a palette with nothing in it")),
        Some(_) => return Err(malformed(next, "neither colours nor a palette, nor the end")),
    };

    let mut plain: String = rows.iter().map(|row| row.iter().collect::<String>() + "\n").collect();
    if !palette_lines.is_empty() {
        plain.push_str(PALETTE_MARKER);
        plain.push('\n');
        for line in palette_lines {
            plain.push_str(line);
            plain.push('\n');
        }
    }
    let rebuilt = document::parse(&plain)
        .ok()
        .filter(|doc| doc.canvas.height() == rows.len())
        .ok_or(malformed(next, "what it holds is no drawing"))?;
    let (mut canvas, palette) = (rebuilt.canvas, rebuilt.palette);
    for (y, line) in colour_lines.iter().enumerate() {
        let at = 1 + glyph_lines + 1 + y;
        paint(&mut canvas, y, rows[y].len(), line, &palette).map_err(|why| malformed(at, why))?;
    }
    Ok(document::render(&canvas, &palette))
}

/// The table line read back — see [`table`] — as a decoder and the characters its glyph
/// numbers stand for. `None` if it is not one, or its lengths are no complete code.
fn read_table(line: &str) -> Option<(Decoder, Vec<char>)> {
    let digits = line.as_bytes();
    let (run, end) = (base64::value(*digits.first()?)?, base64::value(*digits.get(1)?)?);
    let mut lengths = vec![run, end];
    let mut alphabet = Vec::new();
    let mut rest = &line[2..];
    let mut before = 0u64;
    while !rest.is_empty() {
        let mut varints = Varints::new(rest);
        let step = varints.next()?.ok()?;
        let code_point = before.checked_add(step)?;
        // Each character past the one before, so none can be listed twice.
        if !alphabet.is_empty() && step == 0 {
            return None;
        }
        // A control character is never a glyph, and an escape, laid in a row, would be read back
        // as the start of a colour.
        let glyph = char::from_u32(u32::try_from(code_point).ok()?).filter(|g| !g.is_control())?;
        let used = rest.len() - varints.remaining();
        let length = base64::value(*rest.as_bytes().get(used)?)?;
        if length == 0 {
            return None;
        }
        alphabet.push(glyph);
        lengths.push(length);
        before = code_point;
        rest = &rest[used + 1..];
    }
    Some((Decoder::new(&lengths)?, alphabet))
}

/// A row's characters, read back out of its line — an empty line is an empty row. Each run is
/// measured against the room the row has left BEFORE it is laid down, so a damaged or hostile
/// length is refused rather than built.
fn read_glyphs(
    line: &str,
    decoder: &Decoder,
    alphabet: &[char],
) -> Result<Vec<char>, &'static str> {
    let mut row = Vec::new();
    if line.is_empty() {
        return Ok(row);
    }
    let mut bits = Reader::new(line).ok_or("not base64")?;
    loop {
        match decoder.decode(&mut bits).ok_or("bits that are no token's code")? {
            END => break,
            RUN => {
                let before = *row.last().ok_or("a run with no character before it")?;
                let more = bits.gamma(RUN_BITS).ok_or("a run with no length")?;
                if more + 1 > (Size::ASK_ABOVE - row.len()) as u64 {
                    return Err("a row longer than any crumpled so");
                }
                row.extend(std::iter::repeat_n(before, more as usize + 1));
            }
            glyph => {
                if row.len() == Size::ASK_ABOVE {
                    return Err("a row longer than any crumpled so");
                }
                row.push(alphabet[glyph - FIRST_GLYPH]);
            }
        }
    }
    if row.is_empty() {
        return Err("an END with nothing before it, where an empty row is an empty line");
    }
    if !bits.only_filling_left() {
        return Err("more after the END than the zeros that fill it out");
    }
    Ok(row)
}

/// Lay row `y`'s colours from its colour line onto `canvas` — the row `length` characters long.
fn paint(
    canvas: &mut Canvas,
    y: usize,
    length: usize,
    line: &str,
    palette: &crate::Palette,
) -> Result<(), &'static str> {
    let numbers: Vec<u64> =
        Varints::new(line).collect::<Result<_, _>>().map_err(|()| "not varints")?;
    if !numbers.len().is_multiple_of(2) {
        return Err("a colour with no length");
    }
    let (mut x, mut last) = (0usize, None);
    for pair in numbers.chunks(2) {
        let (number, run) = (pair[0], pair[1]);
        if run == 0 || last == Some(number) {
            return Err("runs that are not as a writer leaves them");
        }
        if run > (length - x) as u64 {
            return Err("colour past the end of its row");
        }
        let ink = match number {
            0 => None,
            number => {
                let swatch = palette.at(number as usize - 1).ok_or("no such palette entry")?;
                Some(swatch.color())
            }
        };
        for cell in x..x + run as usize {
            canvas.cell_mut(cell, y).expect("inside the row").ink = ink;
        }
        (x, last) = (x + run as usize, Some(number));
    }
    if last == Some(0) {
        return Err("an uncoloured run at the end, which is left off");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::{crumple, header, uncrumple};
    use super::*;
    use crate::{Palette, Picker, Rgb};

    /// A small seeded generator, so a failing drawing can be made again from its seed.
    struct XorShift(u64);

    impl XorShift {
        fn new(seed: u64) -> Self {
            Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
        }

        fn below(&mut self, n: usize) -> usize {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 % n as u64) as usize
        }
    }

    const ASCII: &[char] = &[' ', ' ', ' ', '#', '#', '.', 'o', '0', '9', '~', '='];
    const BRAILLE: &[char] = &['\u{2800}', '\u{28ff}', '\u{2847}', '\u{28b8}', '\u{2880}', ' '];
    const BLOCKS: &[char] = &['\u{2588}', '\u{2591}', '\u{2592}', '\u{2593}', ' ', 'd'];

    /// A drawing as a picker leaves one: rows of `glyphs`, ragged, some empty, with runs; a palette
    /// of `swatches` colours; and a stretch of each row inked, or not.
    fn drawing(seed: u64, glyphs: &[char], swatches: usize) -> (Canvas, Palette) {
        sized(seed, glyphs, swatches, 1 + seed as usize % 12, 1 + seed as usize % 40)
    }

    /// A [`drawing`] always large enough — 24 rows of up to 60 — that version 1 is the smaller.
    fn sizeable(seed: u64, glyphs: &[char], swatches: usize) -> (Canvas, Palette) {
        sized(seed, glyphs, swatches, 24, 60)
    }

    fn sized(
        seed: u64,
        glyphs: &[char],
        swatches: usize,
        height: usize,
        width: usize,
    ) -> (Canvas, Palette) {
        let mut rng = XorShift::new(seed);
        let rows: Vec<String> = (0..height)
            .map(|_| {
                let mut row = String::new();
                while row.chars().count() < rng.below(width + 1) {
                    let glyph = glyphs[rng.below(glyphs.len())];
                    let run = if rng.below(4) == 0 { 1 + rng.below(9) } else { 1 };
                    row.extend(std::iter::repeat_n(glyph, run));
                }
                row
            })
            .collect();
        // At least one character that is not a space, or there is no drawing.
        let text = format!("x{}", rows.join("\n"));
        let mut canvas = Canvas::from_text(&text).expect("valid glyphs");
        let mut palette = Palette::new();
        for n in 0..swatches {
            let colour = Rgb::new(n as u8, (n * 7) as u8, 200);
            palette.push(format!("colour {n}"), colour).expect("distinct colours");
        }
        let inks: Vec<Ink> = palette.iter().map(|swatch| swatch.color()).collect();
        for y in 0..canvas.height() {
            if inks.is_empty() || rng.below(3) == 0 {
                continue;
            }
            let (from, to) = (rng.below(canvas.width()), rng.below(canvas.width() + 1));
            for x in from..to.max(from) {
                let ink = inks[rng.below(inks.len())];
                canvas.cell_mut(x, y).expect("inside").ink = Some(ink);
            }
        }
        (canvas, palette)
    }

    fn written(canvas: &Canvas, palette: &Palette) -> String {
        document::render(canvas, palette)
    }

    /// `text` crumpled, which must be as version 1.
    fn in_version_1(text: &str) -> String {
        let crumpled = crumple(text);
        assert!(crumpled.starts_with(&header(1)), "not version 1: {crumpled:?}");
        crumpled
    }

    /// The lines at which two crumpled forms of as many lines differ.
    fn changed(before: &str, after: &str) -> Vec<usize> {
        let (before, after): (Vec<&str>, Vec<&str>) =
            (before.lines().collect(), after.lines().collect());
        assert_eq!(before.len(), after.len(), "the same shape");
        (0..before.len()).filter(|&at| before[at] != after[at]).collect()
    }

    /// Drawings of every kind come back from version 1 byte for byte: ASCII, braille and block
    /// characters; no colours, a few, and more than one base64 digit can number.
    #[test]
    fn drawings_come_back_from_version_1_exactly() {
        for seed in 1..=300u64 {
            let glyphs = [ASCII, BRAILLE, BLOCKS][seed as usize % 3];
            let swatches = [0, 1, 5, 70][seed as usize % 4];
            let (canvas, palette) = drawing(seed, glyphs, swatches);
            let text = written(&canvas, &palette);
            let crumpled = crumple(&text);
            assert_eq!(uncrumple(&crumpled).as_deref(), Ok(text.as_str()), "seed {seed}");
        }
    }

    /// Version 1 is its header, the table, a line for each row's characters — then, if anything
    /// is coloured, a marker and a line for each row's colours, and, if there is a palette, a
    /// marker and its lines as the document has them. Every plane line is base64 alone.
    #[test]
    fn version_1_is_a_table_then_a_line_a_row_in_each_plane() {
        let (canvas, palette) = sizeable(7, ASCII, 3);
        let text = written(&canvas, &palette);
        let crumpled = in_version_1(&text);
        let lines: Vec<&str> = crumpled.lines().collect();
        let rows = canvas.height();
        let base64 = |line: &str| line.bytes().all(|c| base64::value(c).is_some());
        assert!(lines[2..2 + rows].iter().all(|line| base64(line)), "{lines:?}");
        assert_eq!(lines[2 + rows], COLOURS);
        assert!(lines[3 + rows..3 + 2 * rows].iter().all(|line| base64(line)));
        assert_eq!(lines[3 + 2 * rows], PALETTE);
        let palette_lines: Vec<&str> = text.lines().skip(rows + 1).collect();
        assert_eq!(lines[4 + 2 * rows..], palette_lines[..], "the palette as it is");
    }

    /// Painting a row changes that row's colour line and nothing else — the change a picker
    /// makes most, and the reason the colours are numbers into the palette.
    #[test]
    fn painting_a_row_changes_its_colour_line_alone() {
        for seed in 1..=40u64 {
            let (mut canvas, palette) = sizeable(seed, BLOCKS, 4);
            let before = in_version_1(&written(&canvas, &palette));
            assert!(before.lines().any(|line| line == COLOURS), "a coloured drawing");
            let y = seed as usize % canvas.height();
            let length = document::written_rows(&canvas)[y].len();
            if length == 0 {
                continue;
            }
            let ink = palette.at(seed as usize % palette.len()).unwrap().color();
            canvas.cell_mut(length - 1, y).unwrap().ink = Some(ink);
            canvas.cell_mut(0, y).unwrap().ink = None;
            let after = in_version_1(&written(&canvas, &palette));
            let colour_line = 2 + canvas.height() + 1 + y;
            let lines = changed(&before, &after);
            assert!(lines.is_empty() || lines == [colour_line], "seed {seed}: {lines:?}");
        }
    }

    /// A swatch recoloured or renamed changes its palette line alone — every cell of that colour
    /// moves with it in the document, and not a line of the planes does.
    #[test]
    fn recolouring_or_renaming_a_swatch_changes_its_palette_line_alone() {
        let (canvas, palette) = sizeable(11, ASCII, 3);
        let mut picker = Picker::new(canvas).with_palette(palette);
        let crumpled = |picker: &Picker| in_version_1(&written(picker.canvas(), picker.palette()));
        let before = crumpled(&picker);
        let last = before.lines().count() - 1;
        picker.set_swatch_color("colour 2", Rgb::new(1, 2, 3)).expect("a free colour");
        assert_eq!(changed(&before, &crumpled(&picker)), [last], "the recoloured swatch's line");
        let mut renamed = picker.palette().clone();
        renamed.rename("colour 2", "sky").expect("a free name");
        let after = in_version_1(&written(picker.canvas(), &renamed));
        assert_eq!(changed(&crumpled(&picker), &after), [last], "the renamed swatch's line");
    }

    /// A swatch added is a line added at the end, and every other line stays as it was — the
    /// palette's order is only ever added to.
    #[test]
    fn a_swatch_added_is_a_line_added() {
        let (canvas, mut palette) = sizeable(13, BRAILLE, 2);
        let before = in_version_1(&written(&canvas, &palette));
        palette.push("new", Rgb::new(9, 9, 9)).expect("a free colour");
        let after = in_version_1(&written(&canvas, &palette));
        assert!(after.starts_with(&before), "{before:?} -> {after:?}");
        assert_eq!(&after[before.len()..], "new\t#090909\n");
    }

    /// Two characters of a row swapped — the drawing's counts of each unchanged, so its table
    /// too — changes that row's line alone.
    #[test]
    fn two_characters_swapped_change_their_rows_line_alone() {
        let (mut canvas, palette) = sizeable(17, ASCII, 0);
        let before = in_version_1(&written(&canvas, &palette));
        let (y, x) = (0..canvas.height())
            .find_map(|y| {
                let row = document::written_rows(&canvas)[y];
                (1..row.len()).find(|&x| row[x].glyph != row[x - 1].glyph).map(|x| (y, x))
            })
            .expect("a row with two characters side by side that differ");
        let (a, b) = (canvas.cell(x - 1, y).unwrap().glyph, canvas.cell(x, y).unwrap().glyph);
        canvas.cell_mut(x - 1, y).unwrap().glyph = b;
        canvas.cell_mut(x, y).unwrap().glyph = a;
        let after = in_version_1(&written(&canvas, &palette));
        assert_eq!(changed(&before, &after), [2 + y], "row {y}'s line, after the table");
    }

    /// A braille drawing's table costs two bytes a character — its place past the one before and
    /// its code length — where the characters themselves are three bytes each.
    #[test]
    fn a_braille_drawings_table_costs_two_bytes_a_character() {
        let braille = ['\u{2801}', '\u{2802}', '\u{2804}', '\u{2808}', '\u{2810}', '\u{2820}'];
        let (canvas, palette) = sizeable(5, &braille, 0);
        let crumpled = in_version_1(&written(&canvas, &palette));
        let table = crumpled.lines().nth(1).expect("a table");
        // Two bytes each, and a byte or two more for each jump to a distant block: from nothing
        // to the `x` every drawing here starts with, and from it to the braille.
        let distinct = braille.len() + 1;
        assert!(table.len() <= 2 + 2 * distinct + 3, "{table:?}");
    }

    // ---- what no writer of version 1 would write ------------------------------------------

    /// A table made by the table's own writer: RUN a one-bit code (`0`), END two (`10`), and
    /// `a` two (`11`).
    fn by_hand_table() -> String {
        table(&[1, 2, 2], &['a'])
    }

    /// A crumpled form of one row, the row's bits given as (value, count) pairs, then `rest`.
    fn by_hand(bits: &[(u64, u32)], gamma: Option<u64>, rest: &str) -> String {
        let mut writer = Writer::default();
        for (at, &(value, count)) in bits.iter().enumerate() {
            writer.bits(value, count);
            if at == 1 {
                if let Some(m) = gamma {
                    writer.gamma(m);
                }
            }
        }
        format!("{}\n{}\n{}\n{rest}", header(1), by_hand_table(), writer.finish())
    }

    fn refused(crumpled: &str) -> (usize, &'static str) {
        match uncrumple(crumpled) {
            Err(UncrumpleError::Malformed { line, why }) => (line, why),
            other => panic!("{crumpled:?} was not refused as damaged: {other:?}"),
        }
    }

    /// The hand-made table reads, and a row made with it comes back — so each refusal below is
    /// the damage it names, not a table that never worked.
    #[test]
    fn the_hand_made_table_reads() {
        let row = by_hand(&[(0b11, 2), (0, 1), (0b10, 2)], Some(1), ""); // a, RUN 1, END: "aaa"
        assert_eq!(uncrumple(&row).as_deref(), Ok("aaa\n"));
    }

    /// Each kind of damage is refused, and says on which line.
    #[test]
    fn what_no_writer_would_write_is_refused_by_line() {
        let a_row = [(0b11, 2), (0b10, 2)]; // "a", END
        let table = by_hand_table();
        let rows = [
            (by_hand(&[(0b11, 2), (0, 1), (0b10, 2)], Some(20_000), ""), 3, "longer than any"),
            (by_hand(&[(0, 1), (0b11, 2), (0b10, 2)], None, ""), 3, "no character before it"),
            (by_hand(&[(0b11, 2), (0, 1)], None, ""), 3, "a run with no length"),
            (by_hand(&[(0b10, 2)], None, ""), 3, "an END with nothing before it"),
            (by_hand(&[(0b11, 2), (0b10, 2), (0b1, 1)], None, ""), 3, "more after the END"),
            (by_hand(&[(0b11_1111, 6)], None, ""), 3, "no token's code"),
            (format!("{}\n{table}\n?\n", header(1)), 3, "not base64"),
            (format!("{}\n", header(1)), 2, "no table line"),
            (format!("{}\n=\n", header(1)), 2, "not a table"),
            (format!("{}\nBCbC\n", header(1)), 2, "not a table"), // ESC, as a character
            (format!("{}\n{}\n", header(1), super::table(&[1, 2, 3], &['a'])), 2, "not a table"),
            (by_hand(&a_row, None, "=== nothing ===\n"), 4, "not base64"),
            (by_hand(&a_row, None, "=== colours ===\n\n=== nothing ===\n"), 6, "neither colours"),
            (by_hand(&a_row, None, "=== palette ===\n"), 4, "nothing in it"),
            (by_hand(&a_row, None, "=== colours ===\n"), 4, "fewer colour lines"),
            (by_hand(&a_row, None, "=== colours ===\nBB\n"), 5, "no such palette"),
        ];
        for (crumpled, line, says) in rows {
            let (at, why) = refused(&crumpled);
            assert!(why.contains(says), "{crumpled:?}: line {at}, {why:?}");
            assert_eq!(at, line, "{crumpled:?}: {why}");
        }
    }

    /// Colours are refused past their row, as a zero-length run or an uncoloured one at the end —
    /// neither of which a writer leaves — and against a palette entry that is not there.
    #[test]
    fn colour_runs_a_writer_would_not_write_are_refused() {
        let palette = "=== palette ===\nred\t#ff0000\n";
        let two = [(0b11, 2), (0b11, 2), (0b10, 2)]; // "aa", END
        let with =
            |colours: &str| by_hand(&two, None, &format!("=== colours ===\n{colours}\n{palette}"));
        // Red for both: what a writer writes, and the drawing it is.
        let red = "\x1b[38;2;255;0;0maa\x1b[0m\n=== arterminal palette ===\nred\t#ff0000\n";
        assert_eq!(uncrumple(&with("BC")).as_deref(), Ok(red));
        for (colours, says) in [
            ("BD", "past the end"),                   // red for three, on a row of two
            ("BA", "not as a writer"),                // red for none
            ("ABAB", "not as a writer"),              // uncoloured, then uncoloured again
            ("BBAB", "an uncoloured run at the end"), // which is left off
            ("CB", "no such palette"),                // the second entry, of one
            ("B", "no length"),
        ] {
            let (_, why) = refused(&with(colours));
            assert!(why.contains(says), "{colours:?}: {why:?}");
        }
    }

    /// More rows than any drawing crumpled so could have is refused without a row being built.
    #[test]
    fn too_many_rows_are_refused() {
        let rows = "\n".repeat(Size::ASK_ABOVE + 2);
        let crumpled = format!("{}\n{}\n{rows}", header(1), by_hand_table());
        assert!(refused(&crumpled).1.contains("more rows"));
    }

    /// Damage of any kind — a character changed, a line lost or doubled, the end cut off — never
    /// panics, and never builds a drawing past the size line.
    #[test]
    fn damaged_crumpled_forms_never_panic_nor_grow_past_the_size_line() {
        let noise = ['A', 'B', '_', '-', '9', 'f', '=', '\n', '~', '\u{e9}', '\t', '#'];
        for seed in 1..=60u64 {
            let (canvas, palette) =
                drawing(seed, [ASCII, BRAILLE, BLOCKS][seed as usize % 3], seed as usize % 5);
            let crumpled = crumple(&written(&canvas, &palette));
            let mut rng = XorShift::new(seed);
            for _ in 0..40 {
                let mut chars: Vec<char> = crumpled.chars().collect();
                let at = rng.below(chars.len());
                match rng.below(4) {
                    0 => chars[at] = noise[rng.below(noise.len())],
                    1 => {
                        chars.remove(at);
                    }
                    2 => chars.truncate(at),
                    _ => chars.insert(at, noise[rng.below(noise.len())]),
                }
                let damaged: String = chars.into_iter().collect();
                if let Ok(text) = uncrumple(&damaged) {
                    assert!(!document::measure(&text).is_large(), "seed {seed}: {damaged:?}");
                }
            }
        }
    }
}
