//! The drawing itself: a rectangular grid of cells, and the loader that reads one from text.

use crate::color::Rgb;
use std::fmt;
use std::path::{Path, PathBuf};

/// One cell of a canvas: the character drawn there, and the colour it is drawn in.
///
/// `ink` is `None` for "whatever the terminal's default foreground is" — which is every cell of
/// a canvas loaded from plain text, since plain text carries no colour.
///
/// It holds the [`Rgb`] itself rather than a reference to a swatch, which is what makes a canvas
/// printable as it stands. Editing a swatch still recolours the drawing, but by rewriting every
/// cell holding that colour rather than by indirection — see
/// [`Picker::set_swatch_color`](crate::Picker::set_swatch_color). That trade is sound only while
/// no two swatches share a colour, which is why [`crate::palette`] forbids it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cell {
    pub glyph: char,
    pub ink: Option<Rgb>,
}

impl Cell {
    /// An uncoloured cell holding `glyph`.
    pub const fn new(glyph: char) -> Self {
        Self { glyph, ink: None }
    }

    /// The empty cell — a space, no ink. What short rows are padded with.
    pub const BLANK: Self = Self::new(' ');
}

/// A rectangular grid of [`Cell`]s.
///
/// Rectangular is an invariant, not a coincidence: every row has exactly [`Canvas::width`] cells,
/// which is what lets a cursor move up and down a column without having to ask whether the cell
/// it is heading for exists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Canvas {
    width: usize,
    height: usize,
    /// Row-major, `width * height` long. Private because that invariant is the whole type.
    cells: Vec<Cell>,
}

impl Canvas {
    /// Read a canvas from plain text: one line per row, one character per cell, no colour.
    ///
    /// The rules, each of which is a decision rather than an accident:
    ///
    /// * **Short rows are padded with spaces** to the width of the longest. Art files are edited
    ///   by hand and hand-editing strips trailing spaces; a ragged grid would mean a cursor that
    ///   can walk off the end of some rows and not others.
    /// * **A trailing newline does not make a final empty row.** `"ab\n"` is one row, because
    ///   every text editor writes that newline and nobody means a blank line by it.
    /// * **`\r\n` is accepted** and the `\r` discarded, so a file written on Windows loads.
    /// * **Control characters are refused**, tabs included. A tab has no single-cell width — it
    ///   means "advance to the next stop", which depends on where the text already is — so a
    ///   canvas containing one cannot say what is in a given column.
    /// * **Every glyph must occupy exactly one terminal column.** See [`CanvasError::WideGlyph`].
    /// * **Empty input is an error**, because a canvas with no coordinates is not a drawing and
    ///   would give the cursor nowhere to stand.
    pub fn from_text(text: &str) -> Result<Self, CanvasError> {
        let rows = text
            .strip_suffix('\n')
            .unwrap_or(text)
            .split('\n')
            .map(strip_carriage_return)
            .map(|line| line.chars().map(Cell::new).collect())
            .collect();
        Self::from_rows(rows)
    }

    /// Build a canvas from rows of cells that may already carry colour — the seam a coloured
    /// file loader comes in through. [`Canvas::from_text`] is this with every cell uninked.
    ///
    /// Applies every rule listed on [`Canvas::from_text`] except the line-splitting ones, which
    /// the caller has already done: rows are padded to the widest, every glyph is checked, and
    /// nothing at all is refused as empty.
    pub fn from_rows(mut rows: Vec<Vec<Cell>>) -> Result<Self, CanvasError> {
        for (y, row) in rows.iter().enumerate() {
            for (x, cell) in row.iter().enumerate() {
                let at = At { row: y, column: x };
                if cell.glyph.is_control() {
                    return Err(CanvasError::Control { at, glyph: cell.glyph });
                }
                let width = display_width(cell.glyph);
                if width != 1 {
                    return Err(CanvasError::WideGlyph { at, glyph: cell.glyph, width });
                }
            }
        }

        let width = rows.iter().map(Vec::len).max().unwrap_or(0);
        if width == 0 {
            return Err(CanvasError::Empty);
        }
        let height = rows.len();
        let mut cells = Vec::with_capacity(width * height);
        for row in &mut rows {
            row.resize(width, Cell::BLANK);
            cells.append(row);
        }
        Ok(Self { width, height, cells })
    }

    /// Read a canvas from a file, as [`Canvas::from_text`] reads one from a string.
    ///
    /// The path is carried into the error rather than left to the caller to remember, because
    /// "expected one column, found 2" is unactionable without it.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, LoadError> {
        let path = path.as_ref();
        let at = |source| LoadError { path: path.to_path_buf(), source };
        let text = std::fs::read_to_string(path).map_err(|err| at(LoadCause::Io(err)))?;
        Self::from_text(&text).map_err(|err| at(LoadCause::Canvas(err)))
    }

    pub fn width(&self) -> usize {
        self.width
    }

    pub fn height(&self) -> usize {
        self.height
    }

    /// The cell at `(x, y)`, counting from the top left.
    pub fn cell(&self, x: usize, y: usize) -> Option<&Cell> {
        (x < self.width && y < self.height).then(|| &self.cells[y * self.width + x])
    }

    /// The cell at `(x, y)`, to be changed.
    pub fn cell_mut(&mut self, x: usize, y: usize) -> Option<&mut Cell> {
        (x < self.width && y < self.height).then(|| &mut self.cells[y * self.width + x])
    }

    /// Row `y`, whole. Always exactly [`Canvas::width`] cells long.
    pub fn row(&self, y: usize) -> Option<&[Cell]> {
        (y < self.height).then(|| &self.cells[y * self.width..(y + 1) * self.width])
    }

    /// Every cell, to be changed — what a recolour walks.
    ///
    /// Flat rather than row-by-row because a recolour has no interest in where a cell is, only in
    /// what colour it holds.
    pub fn cells_mut(&mut self) -> impl Iterator<Item = &mut Cell> {
        self.cells.iter_mut()
    }

    /// Every row, top to bottom.
    pub fn rows(&self) -> impl ExactSizeIterator<Item = &[Cell]> {
        self.cells.chunks_exact(self.width)
    }
}

/// A line as the loader sees it, with any Windows carriage return taken off the end.
fn strip_carriage_return(line: &str) -> &str {
    line.strip_suffix('\r').unwrap_or(line)
}

/// How many terminal columns `glyph` occupies.
///
/// Measured with `console`'s own width function rather than a second width dependency: `console`
/// already carries `unicode-width` in its default features and already uses it to decide how wide
/// everything it prints is. Two width implementations in one tree would eventually disagree, and
/// the one that matters is whichever the drawing code uses.
///
/// The character is encoded into a stack buffer rather than a `String`, so validating a large
/// canvas allocates nothing.
fn display_width(glyph: char) -> usize {
    let mut buffer = [0u8; 4];
    console::measure_text_width(glyph.encode_utf8(&mut buffer))
}

/// Where in the source text something went wrong. Counted from zero; [`fmt::Display`] adds one,
/// because the file the reader will open numbers its lines from one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct At {
    pub row: usize,
    pub column: usize,
}

impl fmt::Display for At {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "line {}, column {}", self.row + 1, self.column + 1)
    }
}

/// Why some text could not be read as a canvas.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CanvasError {
    /// Nothing to draw: no lines, or only empty ones.
    Empty,
    /// A control character, which has no fixed width and so no column of its own.
    Control { at: At, glyph: char },
    /// A glyph that is not exactly one terminal column wide.
    ///
    /// Width `2` is the usual cause — emoji, CJK, and anything else the Unicode tables call
    /// Wide. Width `0` means a combining mark, which belongs to the character before it rather
    /// than to a cell of its own.
    ///
    /// Worth knowing, because the folklore points the wrong way: braille (`U+2800`–`U+28FF`) is
    /// width 1 and perfectly safe. The real hazard is East Asian *Ambiguous* width, which covers
    /// the box-drawing and block-element glyphs an art tool uses constantly: they measure 1 here,
    /// but a terminal configured for a CJK locale draws them 2 columns wide, and then the grid
    /// shears. Such terminals are out of scope; this check cannot see them.
    WideGlyph { at: At, glyph: char, width: usize },
}

impl fmt::Display for CanvasError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "no canvas here: the text is empty"),
            Self::Control { at, glyph } => write!(
                f,
                "{at}: {glyph:?} is a control character and has no column of its own \
                 (expand tabs before loading)"
            ),
            Self::WideGlyph { at, glyph, width } => write!(
                f,
                "{at}: {glyph:?} is {width} columns wide, and a canvas cell is exactly one"
            ),
        }
    }
}

impl std::error::Error for CanvasError {}

/// Why a file could not be loaded as a canvas — the path, and what went wrong with it.
#[derive(Debug)]
pub struct LoadError {
    pub path: PathBuf,
    pub source: LoadCause,
}

/// The ways a load fails: the file, or its contents — as plain art, or as a full document.
#[derive(Debug)]
pub enum LoadCause {
    Io(std::io::Error),
    Canvas(CanvasError),
    Document(crate::document::DocumentError),
}

impl fmt::Display for LoadCause {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(err) => err.fmt(f),
            Self::Canvas(err) => err.fmt(f),
            Self::Document(err) => err.fmt(f),
        }
    }
}

/// Always `path: reason`. The path leads, because it is what tells the reader which of several
/// art files they need to go and look at.
impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.path.display(), self.source)
    }
}

impl std::error::Error for LoadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &self.source {
            LoadCause::Io(err) => Some(err),
            LoadCause::Canvas(err) => Some(err),
            LoadCause::Document(err) => Some(err),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn glyphs(canvas: &Canvas) -> Vec<String> {
        canvas.rows().map(|row| row.iter().map(|cell| cell.glyph).collect()).collect()
    }

    #[test]
    fn a_rectangle_of_text_loads_as_a_grid_of_the_same_shape() {
        let canvas = Canvas::from_text("ab\ncd\nef").expect("valid");
        assert_eq!((canvas.width(), canvas.height()), (2, 3));
        assert_eq!(glyphs(&canvas), ["ab", "cd", "ef"]);
        assert_eq!(canvas.cell(0, 0).unwrap().glyph, 'a');
        assert_eq!(canvas.cell(1, 2).unwrap().glyph, 'f');
    }

    /// Cells are counted in CHARACTERS, not bytes — the whole point of the crate is multi-byte
    /// glyphs, and a byte-indexed grid would put the cursor inside one.
    #[test]
    fn multi_byte_glyphs_are_one_cell_each() {
        let canvas = Canvas::from_text("⣿⡇⣿").expect("braille is width 1");
        assert_eq!((canvas.width(), canvas.height()), (3, 1));
        assert_eq!(canvas.cell(1, 0).unwrap().glyph, '⡇');
    }

    /// Braille is the glyph family the example art is made of, and the one folklore wrongly
    /// warns about. Pinned so nobody "fixes" the width check into rejecting it.
    #[test]
    fn the_whole_braille_block_is_accepted() {
        let every: String = (0x2800u32..=0x28FF).filter_map(char::from_u32).collect();
        let canvas = Canvas::from_text(&every).expect("braille is all width 1");
        assert_eq!(canvas.width(), 256);
    }

    #[test]
    fn short_rows_are_padded_with_blanks_so_the_grid_stays_rectangular() {
        let canvas = Canvas::from_text("abcd\nx\n\nyz").expect("valid");
        assert_eq!((canvas.width(), canvas.height()), (4, 4));
        assert_eq!(glyphs(&canvas), ["abcd", "x   ", "    ", "yz  "]);
        assert!(canvas.rows().all(|row| row.len() == canvas.width()));
    }

    #[test]
    fn a_trailing_newline_does_not_add_an_empty_row() {
        assert_eq!(Canvas::from_text("ab\ncd\n").unwrap(), Canvas::from_text("ab\ncd").unwrap());
        assert_eq!(Canvas::from_text("ab\ncd\n").unwrap().height(), 2);
    }

    /// Two trailing newlines DO mean a blank final row — the first ends the last line of art, the
    /// second is a row the author left empty.
    #[test]
    fn a_second_trailing_newline_is_a_row_the_author_meant() {
        let canvas = Canvas::from_text("ab\n\n").expect("valid");
        assert_eq!(canvas.height(), 2);
        assert_eq!(glyphs(&canvas), ["ab", "  "]);
    }

    #[test]
    fn files_written_on_windows_load() {
        let canvas = Canvas::from_text("ab\r\ncd\r\n").expect("valid");
        assert_eq!(glyphs(&canvas), ["ab", "cd"]);
        assert!(!canvas.rows().flatten().any(|cell| cell.glyph == '\r'));
    }

    #[test]
    fn a_canvas_loaded_from_plain_text_has_no_ink_anywhere() {
        let canvas = Canvas::from_text("ab\ncd").expect("valid");
        assert!(canvas.rows().flatten().all(|cell| cell.ink.is_none()));
    }

    #[test]
    fn nothing_to_draw_is_an_error_rather_than_a_canvas_with_no_coordinates() {
        for nothing in ["", "\n", "\n\n\n", "\r\n"] {
            assert_eq!(Canvas::from_text(nothing), Err(CanvasError::Empty), "{nothing:?}");
        }
    }

    /// A file of spaces is not nothing — it is a blank drawing, and it has coordinates.
    #[test]
    fn a_canvas_of_only_spaces_is_still_a_canvas() {
        let canvas = Canvas::from_text("   \n   ").expect("blank, but present");
        assert_eq!((canvas.width(), canvas.height()), (3, 2));
    }

    #[test]
    fn a_tab_is_refused_and_the_error_says_where() {
        let err = Canvas::from_text("ab\nc\td").expect_err("tabs have no column");
        assert_eq!(err, CanvasError::Control { at: At { row: 1, column: 1 }, glyph: '\t' });
        let message = err.to_string();
        assert!(message.contains("line 2, column 2"), "counted from one: {message}");
        assert!(message.contains("expand tabs"), "and says what to do: {message}");
    }

    #[test]
    fn a_double_width_glyph_is_refused_and_the_error_says_how_wide_it_was() {
        let err = Canvas::from_text("ab\ncd界").expect_err("CJK is two columns");
        assert_eq!(
            err,
            CanvasError::WideGlyph { at: At { row: 1, column: 2 }, glyph: '界', width: 2 }
        );
        assert!(err.to_string().contains("2 columns wide"), "{err}");
    }

    /// Zero-width is refused by the same rule and for the same reason: a combining mark belongs
    /// to the character before it, not to a cell of its own.
    #[test]
    fn a_combining_mark_is_refused_too() {
        let err = Canvas::from_text("e\u{0301}").expect_err("a combining acute is width 0");
        assert!(matches!(err, CanvasError::WideGlyph { width: 0, .. }), "{err:?}");
    }

    #[test]
    fn coordinates_outside_the_grid_read_back_as_nothing() {
        let mut canvas = Canvas::from_text("ab\ncd").expect("valid");
        assert_eq!(canvas.cell(2, 0), None, "past the right edge");
        assert_eq!(canvas.cell(0, 2), None, "past the bottom edge");
        assert_eq!(canvas.cell_mut(2, 2), None);
        assert_eq!(canvas.row(2), None);
    }

    #[test]
    fn a_cell_can_be_inked_and_reads_back_inked() {
        let mut palette = crate::Palette::new();
        let ink = crate::Rgb::new(1, 2, 3);
        palette.push("ink", ink).expect("a fresh palette");
        let mut canvas = Canvas::from_text("ab").expect("valid");
        canvas.cell_mut(1, 0).expect("in bounds").ink = Some(ink);
        assert_eq!(canvas.cell(1, 0).unwrap().ink, Some(ink));
        assert_eq!(canvas.cell(0, 0).unwrap().ink, None, "its neighbour is untouched");
    }

    #[test]
    fn a_missing_file_reports_the_path_it_could_not_read() {
        let err = Canvas::load("/nonexistent/art.txt").expect_err("no such file");
        assert!(matches!(err.source, LoadCause::Io(_)));
        assert!(err.to_string().starts_with("/nonexistent/art.txt: "), "{err}");
    }

    #[test]
    fn a_file_that_is_not_a_canvas_reports_the_path_and_the_reason() {
        let path = std::env::temp_dir().join("arterminal-load-test.txt");
        std::fs::write(&path, "ok\nbad\there").expect("temp write");
        let err = Canvas::load(&path).expect_err("contains a tab");
        let _ = std::fs::remove_file(&path);
        assert!(matches!(err.source, LoadCause::Canvas(CanvasError::Control { .. })));
        let message = err.to_string();
        assert!(message.contains("arterminal-load-test.txt"), "{message}");
        assert!(message.contains("line 2"), "{message}");
    }

    #[test]
    fn a_file_that_is_a_canvas_loads_the_same_as_its_text() {
        let path = std::env::temp_dir().join("arterminal-load-ok.txt");
        std::fs::write(&path, "⣿⡇\n⢹⡇\n").expect("temp write");
        let loaded = Canvas::load(&path).expect("valid art");
        let _ = std::fs::remove_file(&path);
        assert_eq!(loaded, Canvas::from_text("⣿⡇\n⢹⡇\n").unwrap());
    }
}
