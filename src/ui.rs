//! The palette picker: what it looks like, what keys do to it, and the loop that runs one.
//!
//! Three layers, and they are separable on purpose:
//!
//! * [`render`] turns a [`Picker`] into lines of text. Pure.
//! * [`apply`] turns a keystroke into a change to that state. Pure — it never touches a file or
//!   a terminal; the one thing a key can ask for that needs either, saving, comes back to the
//!   caller as [`Action::Save`].
//! * [`run`] is the small impure loop that connects the two to a terminal.
//!
//! Everything that decides what appears lives in the first two, so the picker can be tested
//! exhaustively without a terminal anywhere — and so a program that already owns its screen can
//! call [`render`] and [`apply`] itself and composite the result wherever it likes, instead of
//! surrendering the terminal to [`run`].
//!
//! # One rule for anything here that draws
//!
//! **Compose style attributes; never concatenate escape strings.** Build one
//! [`console::Style`] carrying everything a piece of text needs — ink, inversion, dim — and apply
//! it once. Do not take a string that has already been rendered and wrap it in escapes by hand.
//!
//! The reason is that a rendered string CONTAINS `\x1b[0m`, and a reset inside a hand-rolled
//! `\x1b[7m…\x1b[0m` wrapper ends the reverse video partway along the run. The workaround is to
//! re-arm after every embedded reset — which works, and then has to be remembered at every site
//! that wraps. The sibling project this crate borrows its painter from wraps in four places and
//! forgot the re-arm in one of them, in code reachable from its public API by default.
//!
//! Nothing here wraps, so the whole failure is unreachable rather than merely avoided: a swatch
//! row expresses focus as a plain-text gutter mark beside the coloured block instead of around
//! it, and a canvas cell folds ink and inversion into a single style applied to a single `char` —
//! and a `char` cannot contain a reset.
//!
//! THE CHANGE THAT WOULD BREAK THIS is the first feature needing to highlight a MULTI-CELL RUN: a
//! selection rectangle, a focused swatch shown inverted rather than gutter-marked, a status line
//! embedding a swatch preview. The tempting implementation is to wrap the already-rendered run,
//! and that is exactly the bug above arriving here. Re-render the run's cells instead, with the
//! inversion composed into each cell's style. It costs more allocations and it cannot go wrong.

use crate::canvas::{Canvas, Cell, LoadCause, LoadError};
use crate::color::{Rgb, Rng};
use crate::cursor::{Dir, Focus};
use crate::document;
use crate::palette::{Palette, PaletteError, Recolour, Swatch};
use crate::{input, paint};
use console::{Key, Term};
use std::borrow::Cow;
use std::path::{Path, PathBuf};

/// How many lines [`render`] may be given for a terminal of a given height.
///
/// Re-exported here rather than left in the private module that implements it, because it is part
/// of [`render`]'s contract: a caller told to respect a budget must have a way to compute one.
pub use crate::paint::height_budget;

/// A palette, the drawing it colours, and everything a session of editing them needs to know.
#[derive(Debug, Clone)]
pub struct Picker {
    palette: Palette,
    canvas: Canvas,
    /// Kept on the picker rather than taken fresh per press, so that consecutive `[+]`s walk a
    /// sequence instead of re-seeding from the system each time — and so a caller that wants a
    /// reproducible session can supply its own.
    rng: Rng,
    focus: Focus,
    /// The swatch that paints, by label. `None` until one is picked.
    brush: Option<String>,
    history: History,
    /// One line of news for the status row — a save that succeeded or did not, a warning before
    /// discarding work. Shown once and cleared by the next keystroke.
    notice: Option<String>,
    /// Set by a close request that found unsaved work; the next close request goes through, any
    /// other key clears it.
    quit_armed: bool,
    /// Where a save goes. Set by [`Picker::open`], absent for a picker built in code.
    path: Option<PathBuf>,
}

impl Picker {
    /// A picker over `canvas`, with an empty palette and a generator seeded from the system.
    pub fn new(canvas: Canvas) -> Self {
        let palette = Palette::new();
        Self {
            focus: Focus::first(&palette),
            palette,
            canvas,
            rng: Rng::from_entropy(),
            brush: None,
            history: History::default(),
            notice: None,
            quit_armed: false,
            path: None,
        }
    }

    /// Read a document from `path` — art, colours and palette, as [`crate::document`] lays them
    /// out — and remember the path so [`Picker::save`] knows where to go.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, LoadError> {
        let path = path.as_ref();
        let at = |source| LoadError { path: path.to_path_buf(), source };
        let text = std::fs::read_to_string(path).map_err(|err| at(LoadCause::Io(err)))?;
        let doc = document::parse(&text).map_err(|err| at(LoadCause::Document(err)))?;
        let mut picker = Self::new(doc.canvas).with_palette(doc.palette);
        picker.path = Some(path.to_path_buf());
        Ok(picker)
    }

    /// Start from an existing palette rather than an empty one.
    pub fn with_palette(mut self, palette: Palette) -> Self {
        self.palette = palette;
        self.focus = Focus::first(&self.palette);
        self
    }

    /// Draw random colours from `rng`, so a session can be reproduced exactly.
    pub fn with_rng(mut self, rng: Rng) -> Self {
        self.rng = rng;
        self
    }

    pub fn palette(&self) -> &Palette {
        &self.palette
    }

    pub fn canvas(&self) -> &Canvas {
        &self.canvas
    }

    pub fn canvas_mut(&mut self) -> &mut Canvas {
        &mut self.canvas
    }

    pub fn focus(&self) -> Focus {
        self.focus
    }

    /// Put the cursor somewhere — if that somewhere exists. A caller driving [`apply`] from its
    /// own loop may want to place it; a cursor pointing at nothing is refused rather than kept.
    pub fn set_focus(&mut self, focus: Focus) -> bool {
        let exists = focus.exists(&self.palette, &self.canvas);
        if exists {
            self.focus = focus;
        }
        exists
    }

    /// The label of the swatch that paints, once one has been picked.
    pub fn brush(&self) -> Option<&str> {
        self.brush.as_deref()
    }

    /// Whether there is work that has not reached the file.
    pub fn is_dirty(&self) -> bool {
        self.history.is_dirty()
    }

    pub fn notice(&self) -> Option<&str> {
        self.notice.as_deref()
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Make the swatch under the cursor the one that paints. `false` if the cursor is elsewhere
    /// or it already was.
    pub fn select_brush(&mut self) -> bool {
        let Focus::Swatch { at } = self.focus else { return false };
        let Some(label) = self.palette.at(at).map(|swatch| swatch.label().to_string()) else {
            return false;
        };
        if self.brush.as_deref() == Some(&*label) {
            return false;
        }
        self.brush = Some(label);
        true
    }

    /// Paint the cell under the cursor with the brush. `false` — and no edit recorded — when
    /// the cursor is not on a cell, there is no brush, or the cell already holds that colour.
    pub fn paint(&mut self) -> bool {
        let Some(color) =
            self.brush.as_deref().and_then(|b| self.palette.get(b)).map(Swatch::color)
        else {
            return false;
        };
        self.ink_under_cursor(Some(color))
    }

    /// Clear the cell under the cursor back to the terminal's own colour.
    pub fn erase(&mut self) -> bool {
        self.ink_under_cursor(None)
    }

    fn ink_under_cursor(&mut self, to: Option<Rgb>) -> bool {
        let Focus::Cell { x, y } = self.focus else { return false };
        let Some(cell) = self.canvas.cell_mut(x, y) else { return false };
        let from = cell.ink;
        if from == to {
            return false;
        }
        cell.ink = to;
        self.history.push(Edit::Paint { x, y, from, to });
        true
    }

    /// Append a random swatch under a generated name — what the `[+]` row does.
    ///
    /// Fails only when every colour a random draw can reach is already in the palette; see
    /// [`Palette::push_random`].
    pub fn add_random_swatch(&mut self) -> Result<String, PaletteError> {
        let label = self.palette.push_random(&mut self.rng)?;
        let color = self.palette.get(&label).expect("just added").color();
        self.history.push(Edit::AddSwatch { label: label.clone(), color });
        Ok(label)
    }

    /// Take back the last edit. `false` when there is none.
    pub fn undo(&mut self) -> bool {
        let Some(edit) = self.history.done.pop() else { return false };
        self.reverse(&edit);
        self.history.undone.push(edit);
        true
    }

    /// Put back the last edit taken back. `false` when there is none.
    pub fn redo(&mut self) -> bool {
        let Some(edit) = self.history.undone.pop() else { return false };
        self.forward(&edit);
        self.history.done.push(edit);
        true
    }

    fn forward(&mut self, edit: &Edit) {
        match edit {
            Edit::Paint { x, y, to, .. } => {
                self.canvas.cell_mut(*x, *y).expect("the canvas does not resize").ink = *to;
            }
            Edit::AddSwatch { label, color } => {
                self.palette.push(label.clone(), *color).expect("it was free when it was undone");
            }
        }
    }

    fn reverse(&mut self, edit: &Edit) {
        match edit {
            Edit::Paint { x, y, from, .. } => {
                self.canvas.cell_mut(*x, *y).expect("the canvas does not resize").ink = *from;
            }
            Edit::AddSwatch { label, .. } => {
                // Safe without checking the canvas: edits are undone last-in first-out, so every
                // paint made with this swatch has already been undone by the time this is.
                debug_assert!(
                    !self
                        .canvas
                        .rows()
                        .flatten()
                        .any(|cell| { cell.ink == self.palette.get(label).map(Swatch::color) }),
                    "an AddSwatch undone while its colour was still painted"
                );
                self.palette.remove(label).expect("nothing follows a swatch added by the button");
                if self.brush.as_deref() == Some(label) {
                    self.brush = None;
                }
                if !self.focus.exists(&self.palette, &self.canvas) {
                    self.focus = Focus::Add;
                }
            }
        }
    }

    /// Move a swatch to a new colour, and repaint the drawing to match.
    ///
    /// THIS IS WHY A PICKER OWNS BOTH. A canvas cell holds a colour rather than a reference to a
    /// swatch, so moving a swatch is only half the job: every cell holding the old colour has to
    /// be rewritten, and every cell holding the old colour of anything that followed it. The
    /// palette works out that mapping; this applies it. Neither could do it alone, which is what
    /// makes this an operation on the pair rather than a method on either.
    ///
    /// The history is rewritten by the same mapping, so an edit undone afterwards restores the
    /// swatch's colour AS IT NOW IS — restoring the old colour would put a pixel on the canvas
    /// that no swatch owns, which the whole design forbids.
    ///
    /// Refused whole if the move would put two swatches on one colour — nothing is changed in the
    /// palette, the canvas or the history. See [`Palette::set_color`].
    pub fn set_swatch_color(&mut self, label: &str, color: Rgb) -> Result<Recolour, PaletteError> {
        let recolour = self.palette.set_color(label, color)?;
        for (was, now) in recolour.changes() {
            for cell in self.canvas.cells_mut() {
                if cell.ink == Some(*was) {
                    cell.ink = Some(*now);
                }
            }
            self.history.recolour(*was, *now);
        }
        Ok(recolour)
    }

    /// Write the document back to the path it was opened from.
    pub fn save(&mut self) -> std::io::Result<PathBuf> {
        let Some(path) = self.path.clone() else {
            return Err(std::io::Error::other("this picker was not opened from a file"));
        };
        self.save_to(&path)?;
        Ok(path)
    }

    /// Write the document to `path`, and remember it as the place to save from now on.
    pub fn save_to(&mut self, path: impl AsRef<Path>) -> std::io::Result<()> {
        let path = path.as_ref();
        std::fs::write(path, document::render(&self.canvas, &self.palette))?;
        self.path = Some(path.to_path_buf());
        self.history.mark_saved();
        Ok(())
    }
}

/// One thing a session did that it may want to take back.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Edit {
    Paint { x: usize, y: usize, from: Option<Rgb>, to: Option<Rgb> },
    AddSwatch { label: String, color: Rgb },
}

/// What has been done, what has been taken back, and where the file last agreed with it.
#[derive(Debug, Clone)]
struct History {
    done: Vec<Edit>,
    undone: Vec<Edit>,
    /// How many edits were done when the file was last written — `None` once that point can no
    /// longer be returned to, because something was undone past it and then something new done.
    /// Without that, undoing to the save point and editing again could leave `done` the same
    /// LENGTH as when saved with different CONTENT, and the picker would call itself clean.
    saved_at: Option<usize>,
}

impl Default for History {
    /// Nothing done, and that is the saved point: a document as it was opened or built agrees
    /// with its source by definition. `saved_at: None` here would make every fresh document
    /// report itself dirty — which is exactly what happened before this was spelled out, and a
    /// live run found it as "Esc on a just-opened file warns about unsaved work".
    fn default() -> Self {
        Self { done: Vec::new(), undone: Vec::new(), saved_at: Some(0) }
    }
}

impl History {
    fn push(&mut self, edit: Edit) {
        if self.saved_at.is_some_and(|at| at > self.done.len()) {
            self.saved_at = None;
        }
        self.undone.clear();
        self.done.push(edit);
    }

    fn is_dirty(&self) -> bool {
        self.saved_at != Some(self.done.len())
    }

    fn mark_saved(&mut self) {
        self.saved_at = Some(self.done.len());
    }

    /// Carry a swatch's move into every edit that mentions its old colour.
    fn recolour(&mut self, was: Rgb, now: Rgb) {
        let swap = |ink: &mut Option<Rgb>| {
            if *ink == Some(was) {
                *ink = Some(now);
            }
        };
        for edit in self.done.iter_mut().chain(self.undone.iter_mut()) {
            match edit {
                Edit::Paint { from, to, .. } => {
                    swap(from);
                    swap(to);
                }
                Edit::AddSwatch { color, .. } => {
                    if *color == was {
                        *color = now;
                    }
                }
            }
        }
    }
}

/// How a run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Outcome {
    /// The user closed the picker. Whatever they built is on the [`Picker`].
    Closed,
}

/// What a keystroke did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// State or the cursor moved; the frame needs drawing again.
    Redraw,
    /// Nothing changed — and crucially, nothing is repainted. Terminals emit more than
    /// keystrokes (focus reports, stray escape replies, mouse tracking a previous program left
    /// enabled — a stream on every mouse MOVE); repainting per junk event turns an idle picker
    /// into a render loop.
    Ignored,
    /// The user asked for the document to be written. [`apply`] does not do it — it touches no
    /// file — so the caller must, and then say how it went through the picker's notice.
    Save,
    /// The picker should close.
    Close,
}

/// Width of a swatch's colour block, in terminal columns.
///
/// Three is enough to read a colour against its neighbours and short enough that a narrow
/// terminal still has room for the label beside it.
const SWATCH_WIDTH: usize = 3;

/// The gap between a swatch and its `#` label, as the layout was specified.
const LABEL_GAP: &str = "     ";

/// Follows the label of the swatch that paints. Plain ASCII on purpose: anything from the
/// geometric-shapes block would be East Asian Ambiguous width and shear the row on some terminals.
const BRUSH_TAG: &str = "  (brush)";

/// The `[+]` row: press Enter on it to add a colour.
const ADD_BUTTON: &str = "[+]";

/// What marks the row the cursor is on, and what stands in its place elsewhere. Both are the
/// same width, or every row would shift sideways as the cursor passed.
const CURSOR_MARK: &str = "> ";
const NO_MARK: &str = "  ";

/// Marks a line the terminal was too narrow to show whole.
const CLIPPED: &str = "…";

/// The two control bytes a classic terminal sends for the keys the picker binds. `Ctrl+Shift+Z`
/// is deliberately NOT here: a terminal sends the identical byte for it as for `Ctrl+Z`, so the
/// two cannot be told apart and redo lives on `Ctrl+Y` instead, where most editors also put it.
const CTRL_S: char = '\x13';
const CTRL_X: char = '\x18';
const CTRL_Y: char = '\x19';
const CTRL_Z: char = '\x1a';

/// The picker as lines of text: the palette, the `[+]` row, the canvas, a status row, a hint.
///
/// `width` and `height` are the terminal's, in columns and rows — and `height` is a HARD limit
/// this function never exceeds, because a frame taller than the terminal scrolls, and a scrolled
/// frame breaks the painter's cursor arithmetic permanently rather than cosmetically. Pass
/// [`height_budget`] rather than the raw row count: the two differ by one, because every line of
/// a frame ends with a newline, the last one included.
///
/// Neither limit is enforced silently. A canvas too tall to fit ends in a line saying how many
/// rows were dropped, and a line too wide to fit ends in `…` — a drawing that simply stopped
/// would read as data loss.
pub fn render(picker: &Picker, width: usize, height: usize) -> Vec<String> {
    if height == 0 {
        return Vec::new();
    }
    let focus = picker.focus;

    // Assembled top to bottom, so that when something has to go it is the bottom of the canvas —
    // the part a reader is least surprised to lose, and the part a viewport will later restore.
    let mut body = Vec::with_capacity(picker.palette.len() + picker.canvas.height() + 2);
    for (at, swatch) in picker.palette.iter().enumerate() {
        let is_brush = picker.brush.as_deref() == Some(swatch.label());
        body.push(swatch_row(swatch, focus == Focus::Swatch { at }, is_brush));
    }
    body.push(format!("{}{ADD_BUTTON}", mark(focus == Focus::Add)));
    body.push(String::new());
    for (y, row) in picker.canvas.rows().enumerate() {
        body.push(canvas_row(row, focus, y));
    }

    // Two rows are always kept back — the status and the hint: whatever else is too big to show,
    // what state the work is in and how to leave must not be. On a one-row terminal the hint
    // wins, because leaving is the one thing that cannot be guessed.
    let footer: Vec<String> = match height {
        1 => vec![hint()],
        _ => vec![status(picker), hint()],
    };
    let mut lines = fit(body, height - footer.len());
    lines.extend(footer);
    for line in &mut lines {
        clip(line, width);
    }
    lines
}

/// One palette row: the colour, then its label, then whether it paints.
///
/// The block is spaces with a BACKGROUND colour, not a `█` with a foreground one. Two reasons,
/// and the first is the load-bearing one: `█` lives in Unicode's Block Elements, which are East
/// Asian *Ambiguous* width — a terminal configured for a CJK locale draws them two columns wide,
/// and every row of the picker would shear. A space is unambiguously one column everywhere.
/// Secondarily, a filled background is solid in every font, where `█` shows hairline seams in
/// some.
fn swatch_row(swatch: &Swatch, focused: bool, is_brush: bool) -> String {
    let color = swatch.color();
    let block = stderr_style()
        .bg(console::Color::TrueColor(color.r, color.g, color.b))
        .apply_to(" ".repeat(SWATCH_WIDTH));
    let tag = if is_brush { BRUSH_TAG } else { "" };
    format!("{}{block}{LABEL_GAP}# {}{tag}", mark(focused), swatch.label())
}

/// One row of the canvas, with the cursor's cell inverted if it is on this row.
fn canvas_row(row: &[Cell], focus: Focus, y: usize) -> String {
    let cursor = match focus {
        Focus::Cell { x, y: on } if on == y => Some(x),
        _ => None,
    };
    let mut line = String::from(mark(cursor.is_some()));
    for (x, cell) in row.iter().enumerate() {
        match styled_cell(cell, cursor == Some(x)) {
            // The overwhelmingly common case — an uncoloured cell the cursor is not on — costs
            // one `char` and no allocation, which is what keeps a large canvas cheap to draw.
            None => line.push(cell.glyph),
            Some(styled) => line.push_str(&styled),
        }
    }
    line
}

/// A cell with its ink and the cursor applied, or `None` when it needs neither.
fn styled_cell(cell: &Cell, focused: bool) -> Option<String> {
    let ink = cell.ink;
    if ink.is_none() && !focused {
        return None;
    }
    let mut style = stderr_style();
    if let Some(Rgb { r, g, b }) = ink {
        style = style.fg(console::Color::TrueColor(r, g, b));
    }
    if focused {
        style = style.reverse();
    }
    Some(style.apply_to(cell.glyph).to_string())
}

/// A style destined for stderr, which is where the picker draws.
///
/// NOT decoration. `console` emits an escape only when it believes the destination is a terminal,
/// and the stream a bare `Style` asks about is STDOUT. This picker writes to stderr precisely so
/// that stdout stays free for its output — which means that under the composition the binary
/// advertises, `arterminal art.txt > palette.txt`, a default-built style consults a redirected
/// file, concludes there is no terminal, and emits nothing. A colour picker in no colour.
///
/// It is not only colour: `console` writes attributes inside the same gate as foreground and
/// background, so `.dim()` and `.bold()` disappear with them. Every style that reaches the screen
/// must therefore be built here, which
/// `no_style_is_built_outside_the_stderr_helper` enforces by scanning this tree.
///
/// DO NOT "fix" this into `Term::style()`, which returns a `Style` already bound to its target
/// and looks like the version that cannot drift. [`render`] is deliberately pure and holds no
/// `Term` — that is what lets the whole picker be tested without a terminal — and threading one
/// in to reach `Term::style()` would trade the test design for tidiness.
///
/// Callers compositing [`render`] onto some other stream can override the gate with
/// `console::set_colors_enabled_stderr(true)`; a per-picker knob is deliberately not invented
/// until someone needs one.
///
/// PUBLIC so that the advice is followable. `no_style_is_built_outside_the_stderr_helper` tells a
/// contributor to route new styles through here, and the most natural styling anyone will ever
/// add — colouring the binary's error line — lives in `main.rs`, which consumes this crate
/// externally. Were this private, the only way to satisfy the guard from there would be to
/// hand-copy `Style::new().for_stderr()`, and the convention would again be held at N sites by
/// memory. That is the shape of bug this whole apparatus exists to prevent.
pub fn stderr_style() -> console::Style {
    console::Style::new().for_stderr()
}

/// The cursor's gutter mark for a row.
fn mark(focused: bool) -> &'static str {
    match focused {
        true => CURSOR_MARK,
        false => NO_MARK,
    }
}

/// What state the work is in: the brush, whether the file is behind, and any news.
fn status(picker: &Picker) -> String {
    let brush = match picker.brush() {
        Some(label) => format!("brush: {label}"),
        None => "no brush — space on a swatch picks one".to_string(),
    };
    let state = if picker.is_dirty() { " · modified" } else { "" };
    let mut line = stderr_style().dim().apply_to(format!("{brush}{state}")).to_string();
    if let Some(notice) = picker.notice() {
        line.push_str(&stderr_style().bold().apply_to(format!(" · {notice}")).to_string());
    }
    line
}

/// The key hints, under everything.
fn hint() -> String {
    stderr_style()
        .dim()
        .apply_to(
            "↑↓←→ move · space/enter pick or paint · backspace erase · ^S save · ^Z undo · \
             ^Y redo · esc/^X close",
        )
        .to_string()
}

/// At most `budget` lines, with a count of what was dropped in place of the tail.
///
/// The marker costs one of the budgeted lines, deliberately: a canvas that simply stopped at the
/// bottom of the terminal is indistinguishable from a canvas that ended there, and the difference
/// between "this is the whole drawing" and "the rest is off-screen" is the one a reader cannot
/// afford to guess.
fn fit(mut lines: Vec<String>, budget: usize) -> Vec<String> {
    if lines.len() <= budget {
        return lines;
    }
    if budget == 0 {
        return Vec::new();
    }
    let dropped = lines.len() - (budget - 1);
    lines.truncate(budget - 1);
    lines.push(
        stderr_style()
            .dim()
            .apply_to(format!("{NO_MARK}… {dropped} more rows — terminal too short"))
            .to_string(),
    );
    lines
}

/// Trim `line` to `width` display columns in place, marking it when anything was cut.
///
/// Clipping by DISPLAY WIDTH rather than by characters or bytes is the whole point: a wrapped
/// line occupies two physical rows, and then the frame is taller than the painter believes it to
/// be and the cursor arithmetic drifts.
///
/// Cutting a styled line could leave its colour switched on, and the very next thing
/// [`crate::paint::frame`] writes is `\x1b[K` — which on most terminals erases using the CURRENT
/// background. A swatch row clipped mid-colour would then paint itself across the rest of the
/// screen. `truncate_str` closes any style it cuts through, so nothing is re-armed here; the
/// guarantee is pinned by `no_rendered_line_leaves_a_style_switched_on` rather than defended by
/// code, so a change in `console` fails the build instead of leaking colour at runtime.
///
/// `truncate_str` borrows when it changed nothing, which is what keeps an unclipped frame free of
/// a round of pointless allocation.
fn clip(line: &mut String, width: usize) {
    if let Cow::Owned(cut) = console::truncate_str(line, width, CLIPPED) {
        *line = cut;
    }
}

/// The whole keyboard contract, in one testable place:
/// - `↑`/`↓`/`←`/`→` move the cursor; Tab and Shift-Tab are `↓` and `↑`, for hands already there.
/// - `Space` or `Enter` does the one thing the row under the cursor is for: on a swatch it picks
///   it as the brush, on `[+]` it adds a colour, on a cell it paints with the brush.
/// - `Backspace` or `Delete` on a cell clears its colour.
/// - `Ctrl+S` asks for a save — asks, because this function touches no file.
/// - `Ctrl+Z` undoes and `Ctrl+Y` redoes. Not `Ctrl+Shift+Z`: a classic terminal sends the
///   identical byte for it as for `Ctrl+Z`, so the two cannot be told apart.
/// - `Esc` or `Ctrl+X` closes — unless there is unsaved work, in which case the first press only
///   warns and the second discards. `Ctrl+C` closes at once regardless: [`run`] holds the
///   terminal in raw mode, which turns off signal generation, so `Ctrl+C` arrives as an ordinary
///   keystroke and nothing else will ever act on it. A loop that ignored it would be a loop the
///   user cannot interrupt, and an interrupt that argued would not be one.
///
/// Any keystroke clears the last notice, so a warning shown once is not shown for ever.
pub fn apply(picker: &mut Picker, key: Key) -> Action {
    let had_notice = picker.notice.take().is_some();
    let armed = std::mem::take(&mut picker.quit_armed);

    let action = match key {
        Key::CtrlC => Action::Close,
        Key::Escape | Key::Char(CTRL_X) => match picker.is_dirty() && !armed {
            true => {
                picker.quit_armed = true;
                picker.notice =
                    Some("unsaved changes — press again to discard, or ^S to save".to_string());
                Action::Redraw
            }
            false => Action::Close,
        },
        Key::Char(CTRL_S) => Action::Save,
        Key::Char(CTRL_Z) => redraw_if(picker.undo()),
        Key::Char(CTRL_Y) => redraw_if(picker.redo()),
        Key::ArrowUp | Key::BackTab => moved(picker, Dir::Up),
        Key::ArrowDown | Key::Tab => moved(picker, Dir::Down),
        Key::ArrowLeft => moved(picker, Dir::Left),
        Key::ArrowRight => moved(picker, Dir::Right),
        Key::Enter | Key::Char(' ') => match picker.focus {
            Focus::Swatch { .. } => redraw_if(picker.select_brush()),
            // A refusal is not a repaint: the palette is unchanged, so the frame would be
            // identical. It can only happen once a palette has taken the whole ring of colours
            // `Rgb::random` draws from, which is a state a person would have to work at.
            Focus::Add => redraw_if(picker.add_random_swatch().is_ok()),
            Focus::Cell { .. } => redraw_if(picker.paint()),
        },
        Key::Backspace | Key::Del => redraw_if(picker.erase()),
        _ => Action::Ignored,
    };

    // A key that changed nothing still has to repaint if it took a notice or a warning off the
    // status row — the screen would otherwise keep showing what the picker no longer holds.
    match action {
        Action::Ignored if had_notice || armed => Action::Redraw,
        other => other,
    }
}

fn redraw_if(changed: bool) -> Action {
    match changed {
        true => Action::Redraw,
        false => Action::Ignored,
    }
}

/// Move the cursor, reporting whether anything actually changed. An edge that refuses the move
/// is `Ignored`, not `Redraw` — redrawing an identical frame is work nobody asked for.
fn moved(picker: &mut Picker, dir: Dir) -> Action {
    match picker.focus.step(dir, &picker.palette, &picker.canvas) {
        Some(next) => {
            picker.focus = next;
            Action::Redraw
        }
        None => Action::Ignored,
    }
}

/// Show `picker` on the terminal and let the user drive it, until they close it.
///
/// Draws on STDERR, leaving stdout free for a program to print whatever the session produced —
/// the same split the sibling project uses, so `… > out.txt` composes.
///
/// Returns once the user closes the picker, or the terminal goes away. Whatever they built is on
/// the `picker` they lent us, and if they saved it is on disk too.
pub fn run(picker: &mut Picker) -> std::io::Result<Outcome> {
    // BUFFERED, and the difference is the whole of the flicker: see [`crate::paint`]. Same target
    // and same tty-ness as `Term::stderr()` — only the writes are pooled, until an explicit flush.
    let term = Term::buffered_stderr();
    if !term.is_term() {
        return Err(std::io::Error::other("a picker needs a terminal (stderr is not one)"));
    }

    let mut on_screen = 0;
    let mut last_size = term.size();
    let (fd, _tty_handle) = input::input_fd()?;
    // Raw for the whole run (drops — and restores — on every path out of this function).
    let _raw = input::RawMode::engage(fd)?;
    term.hide_cursor()?;
    term.flush()?;

    let outcome = loop {
        // Asked every frame rather than once: a terminal can be resized at any moment, and both
        // limits below are derived from the answer.
        let size = term.size();
        if size != last_size {
            // RE-ANCHOR instead of stepping up over a frame that no longer exists where we left
            // it. A resize reflows the scrollback — a narrowing one turns a wide line into two
            // physical rows — so `on_screen` is no longer the number of rows the old frame
            // occupies, and stepping up by it would land somewhere arbitrary and stay wrong. The
            // cost of re-anchoring is that the old frame is left on screen above the new one.
            // That is visible and self-correcting; miscounted cursor arithmetic is neither.
            on_screen = 0;
            last_size = size;
        }
        let (rows, columns) = size;
        let lines = render(picker, columns as usize, paint::height_budget(rows as usize));
        paint::paint(&term, &lines, on_screen)?;
        on_screen = lines.len();

        // Counted across the whole drain below, not per key: it is the burst that is being
        // bounded, so the tally has to outlive the individual keystrokes it is folding.
        let mut drained = 0;
        let action = loop {
            match input::await_input(fd) {
                Ok(true) => {}
                // Hangup, or an unreadable terminal: nobody is there to answer.
                Ok(false) | Err(_) => break Action::Close,
            }
            // `read_key_raw`, NOT `read_key`. The plain one answers Ctrl+C by raising SIGINT on
            // the process — the terminal is raw, so nothing else would — and the default handler
            // ends the process on the spot, before `RawMode` can drop and put the terminal back.
            // The shell is then left in raw mode with the cursor hidden. The raw variant hands
            // Ctrl+C back as a keystroke, and [`apply`] closes on it like any other.
            match term.read_key_raw() {
                // The terminal went away mid-session (hangup, ctrl-d): treat as closing.
                Err(_) => break Action::Close,
                Ok(key) => {
                    let action = apply(picker, key);
                    drained += 1;
                    // A key that changed nothing never painted anything anyway; a key that did,
                    // with more input already queued behind it, paints once for the whole burst.
                    // This is what stops a held arrow key from queueing a full render each.
                    let redraws = matches!(action, Action::Redraw);
                    if matches!(action, Action::Ignored)
                        || input::coalesce(redraws, input::input_pending(fd), drained)
                    {
                        continue;
                    }
                    break action;
                }
            }
        };
        match action {
            Action::Close => break Outcome::Closed,
            // The one thing a key can ask for that needs a file. Done here, where the file is
            // reachable, and reported back through the picker so the next frame can say so.
            Action::Save => {
                picker.notice = Some(match picker.save() {
                    Ok(path) => format!("saved to {}", path.display()),
                    Err(err) => format!("not saved: {err}"),
                });
            }
            Action::Redraw | Action::Ignored => {}
        }
    };

    // The picker comes off the screen on the way out — through the same one write, so the last
    // thing a user sees is not a block erasing itself a line at a time. A frame of NO lines
    // against a block of `on_screen` wipes every row and leaves the cursor where the picker began.
    paint::paint(&term, &[], on_screen)?;
    term.show_cursor()?;
    term.flush()?;
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::palette::HsbOffset;

    /// Force colour on for these tests.
    ///
    /// `console` disables styling when the stream is not a terminal, and under `cargo test` it
    /// never is — so without this every assertion about an escape sequence would pass vacuously
    /// against plain text.
    ///
    /// This writes ONE of `console`'s two process-wide switches, the stderr one, and always the
    /// same value. That is what makes it safe under the parallel test harness, and it is a
    /// narrower claim than it looks: the switches are `OnceLock<AtomicBool>` statics shared by
    /// every test in this binary, so a test writing the OTHER one would pin it for all the rest.
    /// The single test that must write both — proving styling follows stderr rather than a
    /// redirected stdout — lives in `tests/stderr_gate.rs`, where cargo gives it its own process.
    /// Keep it that way rather than moving it back here.
    fn with_colour() {
        console::set_colors_enabled_stderr(true);
    }

    const RED: Rgb = Rgb::new(226, 57, 57);
    const BLUE: Rgb = Rgb::new(57, 144, 226);

    fn picker(swatches: usize, art: &str) -> Picker {
        let mut palette = Palette::new();
        let mut rng = Rng::from_seed(1);
        for _ in 0..swatches {
            palette.push_random(&mut rng).expect("the ring is not full");
        }
        Picker::new(Canvas::from_text(art).expect("valid art"))
            .with_palette(palette)
            .with_rng(Rng::from_seed(99))
    }

    /// A copy of `picker` with the cursor placed, so a render test can ask about one position
    /// without mutating the fixture the next assertion uses.
    fn at(picker: &Picker, focus: Focus) -> Picker {
        let mut placed = picker.clone();
        assert!(placed.set_focus(focus), "{focus:?} must exist in the fixture");
        placed
    }

    /// Wide and tall enough that nothing below is clipped unless the test means it to be.
    fn roomy(picker: &Picker, focus: Focus) -> Vec<String> {
        render(&at(picker, focus), 200, 60)
    }

    fn marked(lines: &[String]) -> Vec<usize> {
        lines
            .iter()
            .enumerate()
            .filter(|(_, line)| line.starts_with(CURSOR_MARK))
            .map(|(at, _)| at)
            .collect()
    }

    /// Press `keys` in order, returning what the last one did.
    fn press(picker: &mut Picker, keys: &[Key]) -> Action {
        let mut last = Action::Ignored;
        for key in keys {
            last = apply(picker, key.clone());
        }
        last
    }

    #[test]
    fn the_layout_is_the_palette_then_the_button_then_a_gap_then_the_art() {
        with_colour();
        let picker = picker(2, "ab\ncd");
        let lines = roomy(&picker, Focus::Add);
        assert_eq!(lines.len(), 2 + 1 + 1 + 2 + 2, "swatches, button, gap, art, status, hint");
        assert!(lines[2].contains(ADD_BUTTON), "the button follows the swatches: {:?}", lines[2]);
        assert_eq!(lines[3].trim(), "", "a blank row separates the palette from the art");
        assert!(lines[4].contains("ab"), "{:?}", lines[4]);
        assert!(lines[5].contains("cd"), "{:?}", lines[5]);
        assert!(lines[6].contains("brush"), "the status row: {:?}", lines[6]);
        assert!(lines[7].contains("esc"), "the hint last: {:?}", lines[7]);
    }

    #[test]
    fn a_swatch_row_shows_its_colour_as_a_background_block_beside_its_label() {
        with_colour();
        let mut palette = Palette::new();
        palette.push("sky", Rgb::new(30, 144, 255)).expect("fresh");
        let picker = Picker::new(Canvas::from_text("x").expect("valid")).with_palette(palette);
        let row = &roomy(&picker, Focus::Add)[0];

        assert!(row.contains("\x1b[48;2;30;144;255m"), "the colour is a BACKGROUND: {row:?}");
        assert!(!row.contains("\x1b[38;2;30;144;255m"), "not a foreground: {row:?}");
        assert!(row.contains(&format!("{LABEL_GAP}# sky")), "label follows the gap: {row:?}");
        assert!(!row.contains('█'), "the block is spaces, not a block glyph: {row:?}");
    }

    /// One row carries the cursor at a time, wherever it is — checked over the whole surface
    /// rather than at a couple of sampled positions.
    #[test]
    fn exactly_one_row_carries_the_cursor_mark_wherever_the_cursor_is() {
        with_colour();
        let picker = picker(3, "abc\ndef");
        let everywhere = (0..3)
            .map(|at| Focus::Swatch { at })
            .chain([Focus::Add])
            .chain((0..2).flat_map(|y| (0..3).map(move |x| Focus::Cell { x, y })));
        for focus in everywhere {
            assert_eq!(marked(&roomy(&picker, focus)).len(), 1, "{focus:?}");
        }
    }

    #[test]
    fn the_cursor_mark_lands_on_the_row_the_focus_names() {
        with_colour();
        let picker = picker(2, "ab\ncd");
        assert_eq!(marked(&roomy(&picker, Focus::Swatch { at: 0 })), [0]);
        assert_eq!(marked(&roomy(&picker, Focus::Swatch { at: 1 })), [1]);
        assert_eq!(marked(&roomy(&picker, Focus::Add)), [2]);
        assert_eq!(marked(&roomy(&picker, Focus::Cell { x: 1, y: 0 })), [4]);
        assert_eq!(marked(&roomy(&picker, Focus::Cell { x: 1, y: 1 })), [5]);
    }

    #[test]
    fn the_focused_canvas_cell_is_the_only_one_inverted() {
        with_colour();
        let picker = picker(0, "abc");
        let row = &roomy(&picker, Focus::Cell { x: 1, y: 0 })[2];
        let inverted = stderr_style().reverse().apply_to('b').to_string();
        assert!(row.contains(&inverted), "the cursor's cell is inverted: {row:?}");
        assert_eq!(row.matches("\x1b[7m").count(), 1, "and only that one: {row:?}");
        assert!(row.contains(&format!("a{inverted}c")), "neighbours stay plain: {row:?}");
    }

    #[test]
    fn an_inked_cell_is_drawn_in_its_swatch_colour() {
        with_colour();
        let mut palette = Palette::new();
        let ink = Rgb::new(255, 0, 128);
        palette.push("pink", ink).expect("fresh");
        let mut picker = Picker::new(Canvas::from_text("ab").expect("valid")).with_palette(palette);
        picker.canvas_mut().cell_mut(0, 0).expect("in bounds").ink = Some(ink);

        // One swatch, then the button, then the gap: the art starts on row three.
        let row = &roomy(&picker, Focus::Add)[3];
        assert!(row.contains("\x1b[38;2;255;0;128m"), "ink is a FOREGROUND colour: {row:?}");
        assert_eq!(row.matches("\x1b[38;2;").count(), 1, "only the inked cell: {row:?}");
    }

    /// The brush is named on its own row, and nowhere else.
    #[test]
    fn the_swatch_that_paints_is_tagged_on_its_row_and_in_the_status() {
        with_colour();
        let mut tagged = picker(2, "ab");
        tagged.set_focus(Focus::Swatch { at: 1 });
        assert!(tagged.select_brush());
        let lines = roomy(&tagged, Focus::Add);
        assert!(!lines[0].contains(BRUSH_TAG), "{:?}", lines[0]);
        assert!(lines[1].contains(BRUSH_TAG), "{:?}", lines[1]);
        let status = &lines[lines.len() - 2];
        assert!(status.contains("brush: colour 2"), "{status:?}");

        let none = roomy(&picker(1, "ab"), Focus::Add);
        assert!(none[none.len() - 2].contains("no brush"), "{:?}", none[none.len() - 2]);
    }

    /// The hard limit. A frame taller than the terminal scrolls, and a scrolled frame breaks the
    /// painter's cursor arithmetic permanently — so this is swept across every shape rather than
    /// spot-checked.
    #[test]
    fn render_never_returns_more_lines_than_the_height_allows() {
        with_colour();
        for swatches in 0..4 {
            for art_rows in 1..12 {
                let art = vec!["ab"; art_rows].join("\n");
                let picker = picker(swatches, &art);
                for height in 0..20 {
                    let lines = render(&picker, 40, height);
                    assert!(
                        lines.len() <= height,
                        "{swatches} swatches over {art_rows} rows at height {height} gave {}",
                        lines.len()
                    );
                }
            }
        }
    }

    /// The other half of the same rule: a line wider than the terminal wraps onto a second
    /// physical row, and the frame is then taller than the painter counted.
    #[test]
    fn no_rendered_line_is_wider_than_the_terminal() {
        with_colour();
        let picker = picker(3, "⣿⡇⣿⣿⣿⠛⠁⣴⣿⡿⠿⠧⠹⠿⠘⣿⣿⣿⡇⢸⡻⣿⣿⣿⣿⣿⣿⣿\n⢹⡇⣿⣿⣿⠄⣞⣯⣷⣾⣿⣿⣧⡹⡆⡀⠉⢹⡌⠐⢿⣿⣿⣿⡞⣿⣿⣿");
        for width in 1..50 {
            for focus in [Focus::Swatch { at: 0 }, Focus::Add, Focus::Cell { x: 5, y: 1 }] {
                for line in render(&at(&picker, focus), width, 20) {
                    let measured = console::measure_text_width(&line);
                    assert!(
                        measured <= width,
                        "{measured} columns in a {width}-wide terminal: {line:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn a_canvas_too_tall_to_fit_reports_how_many_rows_it_dropped() {
        with_colour();
        let picker = picker(1, &vec!["ab"; 20].join("\n"));
        let lines = render(&picker, 40, 10);
        assert_eq!(lines.len(), 10);

        let marker = &lines[lines.len() - 3];
        assert!(marker.contains("more rows"), "the drop is announced: {marker:?}");
        // 1 swatch + button + gap + 20 art rows = 23 lines of body; the budget of 8 keeps 7.
        assert!(marker.contains("16 more rows"), "and counted: {marker:?}");
    }

    /// However little room there is, how to leave must still be on screen.
    #[test]
    fn the_hint_is_the_last_line_and_survives_the_shortest_terminal() {
        with_colour();
        let picker = picker(2, "ab\ncd");
        for height in 1..12 {
            let lines = render(&picker, 200, height);
            let last = lines.last().expect("at least the hint");
            assert!(last.contains("esc"), "height {height} lost the hint: {last:?}");
        }
    }

    #[test]
    fn a_terminal_with_no_rows_gets_no_frame_rather_than_a_panic() {
        with_colour();
        assert_eq!(render(&picker(2, "ab"), 40, 0), Vec::<String>::new());
    }

    /// Whether a line ends with a colour still switched on.
    ///
    /// Reads the real escape bytes rather than trusting how they were built: the last SGR in the
    /// line must be a reset, or the terminal carries that style into the `\x1b[K` the painter
    /// writes next — which erases using the CURRENT background, painting the colour across the
    /// rest of the screen.
    fn leaves_a_style_open(line: &str) -> bool {
        line.split("\x1b[")
            .skip(1)
            .filter_map(|part| part.split_once('m'))
            .map(|(code, _)| code)
            .last()
            .is_some_and(|last| last != "0")
    }

    /// The invariant behind [`clip`] not re-arming anything itself: `console` closes any style it
    /// cuts through. Swept across every width so the cut lands inside the colour, on its edge,
    /// and past it — and driven through `render`, so it is the real lines that are checked rather
    /// than a hand-built imitation of them.
    #[test]
    fn no_rendered_line_leaves_a_style_switched_on() {
        with_colour();
        let mut picker = picker(2, "ab\ncd");
        let ink = picker.palette().at(0).expect("two swatches").color();
        picker.canvas_mut().cell_mut(0, 0).expect("in bounds").ink = Some(ink);
        picker.notice = Some("a bold notice too".into());
        for width in 1..40 {
            for focus in [Focus::Swatch { at: 0 }, Focus::Add, Focus::Cell { x: 0, y: 0 }] {
                for line in render(&at(&picker, focus), width, 20) {
                    assert!(!leaves_a_style_open(&line), "width {width}: {line:?}");
                }
            }
        }
    }

    /// The helper above has to be able to fail, or the sweep that uses it proves nothing.
    #[test]
    fn an_unclosed_style_is_recognised_as_one() {
        assert!(leaves_a_style_open("\x1b[48;2;1;2;3m   "), "colour left on");
        assert!(!leaves_a_style_open("\x1b[48;2;1;2;3m   \x1b[0m"), "closed");
        assert!(!leaves_a_style_open("\x1b[48;2;1;2;3m \x1b[0m trailing text"), "closed early");
        assert!(!leaves_a_style_open("no escapes at all"), "nothing to leave open");
    }

    /// Every style that reaches the screen must come from [`stderr_style`].
    ///
    /// The behavioural test in `tests/stderr_gate.rs` can only check the styled elements that
    /// exist TODAY. A new one built with a bare `console` style would slip past it and silently
    /// lose its styling under redirection — which is precisely how this shipped in the sibling
    /// project, and what no unit test there could have caught. Scanning the tree turns "someone
    /// remembered" into a failing build.
    ///
    /// Three rules, because the first two alone can be walked around:
    ///
    /// 1. No `Style::new()` or `Style::default()` without `.for_stderr()` on the same line. The
    ///    needle is the BARE spelling on purpose — the qualified `console::Style::new()` contains
    ///    it, so matching the short form catches both, while matching the long one would go blind
    ///    the moment somebody added `Style` to the `use console::{…}` list two lines up.
    ///    `default()` is here because `console`'s `Default for Style` is literally
    ///    `Self::new()` — an identical ungated style, under a name plenty of people reach for
    ///    reflexively. An accident someone will have, not an adversarial spelling.
    /// 2. No bare `console::style(`, the free function, which builds a stdout-gated style.
    /// 3. No `use console::…` importing `style` or `Style`, which is what would let a contributor
    ///    spell either of the above in a form the needles cannot see. Nothing here needs them
    ///    imported, so keeping that surface narrow costs nothing and closes the gap.
    ///
    /// Rule 3 is LOAD-BEARING rather than belt-and-braces, which is easy to miss when trimming a
    /// test: `use console::Style as Styling;` makes the call site `Styling::new()`, which rule 1
    /// cannot see at all. Rule 3 is the only thing that catches an alias, and it catches it at the
    /// import rather than at the use, which is also the better place to be told.
    ///
    /// The needles are split across `concat!`: this test reads its own source, and written whole
    /// they would match themselves.
    ///
    /// # Where this protection stops
    ///
    /// It is a TEXT scan, so it sees only what a source line spells. Put a `console::Style` field
    /// in a struct and `#[derive(Default)]` it, and an ungated style is constructed inside a macro
    /// expansion with no source line naming it anywhere. No rule added here can ever see that.
    /// That is the technique's boundary, not a hole to patch — three rules plus a public
    /// [`stderr_style`] now cover every accident a contributor will plausibly have, and chasing
    /// further spellings is diminishing returns.
    ///
    /// The structural fix, REJECTED and recorded so it is not re-proposed: a newtype
    /// `struct StderrStyle(console::Style)` exposing only values built through [`stderr_style`],
    /// so `console::Style` never appears in this crate's own code and an ungated one is
    /// unconstructible rather than merely un-greppable. It costs a wrapper for every builder
    /// method used — `fg`, `bg`, `reverse`, `dim` — to defend a handful of call sites against a
    /// route that requires storing a `console::Style` in a struct, which nothing in this design
    /// has reason to do. The guard is the right weight for the risk.
    ///
    /// As the code stands that boundary is UNREACHABLE rather than merely known: the precondition
    /// is a `console::Style` living in a struct field, and nothing here has one — the type appears
    /// in code exactly twice, both in [`stderr_style`] itself, and no `#[derive(Default)]` in the
    /// tree is on a struct holding one. So the trigger to watch is single and specific, and a
    /// later audit can confirm the ceiling still does not apply with one grep for a
    /// `console::Style` field. That is a better position than a limitation which is documented
    /// but unverifiable.
    ///
    /// IF THAT EVER CHANGES — if styles start being held in structs rather than built and
    /// consumed in one expression — this guard has stopped covering the case, and whoever is
    /// making that change should know it rather than assume the protection still holds.
    #[test]
    fn no_style_is_built_outside_the_stderr_helper() {
        let constructors = [concat!("Style::", "new()"), concat!("Style::", "default()")];
        let free_fn = concat!("console::", "style(");
        let console_import = "use console::";

        let mut pending = vec![std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src")];
        let mut offenders = Vec::new();
        while let Some(dir) = pending.pop() {
            for entry in std::fs::read_dir(&dir).expect("the source tree is readable") {
                let path = entry.expect("a readable entry").path();
                if path.is_dir() {
                    pending.push(path); // so a future submodule cannot fall outside the guard
                    continue;
                }
                if path.extension().is_none_or(|ext| ext != "rs") {
                    continue;
                }
                let text = std::fs::read_to_string(&path).expect("source is utf-8");
                for (number, line) in text.lines().enumerate() {
                    let code = line.trim_start();
                    if code.starts_with("//") {
                        continue; // prose may name the very thing it warns against
                    }
                    let mut complaint = None;
                    // Says "outside the helper" rather than "gated on stdout" deliberately: a
                    // correct chain that rustfmt wrapped, with `.for_stderr()` on the next line,
                    // trips this too. The firing is right — `stderr_style()` is the sanctioned
                    // path — but calling such a line stdout-gated would be a misdiagnosis.
                    if constructors.iter().any(|needle| code.contains(needle))
                        && !code.contains(".for_stderr()")
                    {
                        complaint = Some("builds a style outside the helper");
                    } else if code.contains(free_fn) {
                        complaint = Some("uses the stdout-gated free function");
                    } else if imports_a_style_name(code, console_import) {
                        complaint = Some("imports a style name, which hides the two rules above");
                    }
                    if let Some(why) = complaint {
                        offenders.push(format!("{}:{} {why}", path.display(), number + 1));
                    }
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "the picker draws on stderr, so a style gated on stdout renders blank under a \
             redirect — build it with arterminal::ui::stderr_style(): {offenders:?}"
        );
    }

    /// Whether `code` is a `use console::…` bringing `style` or `Style` into scope.
    ///
    /// Split out so the alias and brace-list spellings are handled in one place rather than
    /// inline: `use console::Style;`, `use console::{Key, Style};` and `use console::Style as S;`
    /// all have to be caught, and each would otherwise be a separate condition to forget.
    fn imports_a_style_name(code: &str, prefix: &str) -> bool {
        let Some(names) = code.strip_prefix(prefix) else { return false };
        let names = names.trim().trim_end_matches(';').trim();
        let names =
            names.strip_prefix('{').and_then(|rest| rest.strip_suffix('}')).unwrap_or(names);
        names
            .split(',')
            .filter_map(|name| name.split_whitespace().next())
            .any(|name| name == "style" || name == "Style")
    }

    #[test]
    fn a_clipped_line_says_it_was_clipped() {
        let mut line = String::from("abcdefghij");
        clip(&mut line, 5);
        assert!(line.contains(CLIPPED), "{line:?}");
        assert_eq!(console::measure_text_width(&line), 5);
    }

    #[test]
    fn a_line_that_fits_is_left_exactly_as_it_was() {
        let original = "\x1b[48;2;1;2;3m   \x1b[0m short";
        let mut line = String::from(original);
        clip(&mut line, 80);
        assert_eq!(line, original, "nothing was cut, so nothing changed");
    }

    // ---- what keys do -----------------------------------------------------------------------

    #[test]
    fn enter_on_the_button_adds_a_colour() {
        let mut picker = picker(0, "ab");
        assert_eq!(picker.focus(), Focus::Add, "with no swatches the button is the top row");
        assert_eq!(apply(&mut picker, Key::Enter), Action::Redraw);
        assert_eq!(picker.palette().len(), 1);
        assert_eq!(picker.focus(), Focus::Add, "and the cursor stays on the button");
    }

    #[test]
    fn every_press_of_the_button_adds_another_distinct_colour() {
        let mut picker = picker(0, "ab");
        for _ in 0..8 {
            apply(&mut picker, Key::Enter);
        }
        assert_eq!(picker.palette().len(), 8);
        let colours: std::collections::HashSet<_> =
            picker.palette().iter().map(Swatch::color).collect();
        assert_eq!(colours.len(), 8, "consecutive presses must not repeat a colour");
    }

    /// Space and Enter mean the same thing everywhere: do what the row under the cursor is for.
    #[test]
    fn space_and_enter_are_interchangeable() {
        let mut picker = picker(1, "ab");
        picker.set_focus(Focus::Swatch { at: 0 });
        assert_eq!(apply(&mut picker, Key::Char(' ')), Action::Redraw, "space picks the brush");
        assert_eq!(picker.brush(), Some("colour 1"));
        picker.set_focus(Focus::Cell { x: 0, y: 0 });
        assert_eq!(apply(&mut picker, Key::Enter), Action::Redraw, "enter paints with it");
        assert_eq!(
            picker.canvas().cell(0, 0).unwrap().ink,
            Some(picker.palette().at(0).unwrap().color())
        );
    }

    /// The core loop of the tool: pick a swatch, walk to a cell, put the colour down.
    #[test]
    fn picking_a_swatch_then_confirming_on_a_cell_paints_it() {
        let mut picker = picker(2, "ab\ncd");
        let second = picker.palette().at(1).unwrap().color();
        picker.set_focus(Focus::Swatch { at: 1 });
        press(&mut picker, &[Key::Enter]);
        assert_eq!(picker.brush(), Some("colour 2"));

        // Down past [+], then into the art, then right once.
        press(&mut picker, &[Key::ArrowDown, Key::ArrowDown, Key::ArrowRight]);
        assert_eq!(picker.focus(), Focus::Cell { x: 1, y: 0 });
        assert_eq!(apply(&mut picker, Key::Enter), Action::Redraw);
        assert_eq!(picker.canvas().cell(1, 0).unwrap().ink, Some(second));
        assert_eq!(picker.canvas().cell(0, 0).unwrap().ink, None, "only the cell under the cursor");
        assert!(picker.is_dirty());
    }

    #[test]
    fn painting_needs_a_brush_and_a_change() {
        let mut picker = picker(1, "ab");
        picker.set_focus(Focus::Cell { x: 0, y: 0 });
        assert_eq!(apply(&mut picker, Key::Enter), Action::Ignored, "no brush picked yet");
        assert!(!picker.is_dirty(), "and nothing was recorded");

        picker.set_focus(Focus::Swatch { at: 0 });
        press(&mut picker, &[Key::Enter]);
        picker.set_focus(Focus::Cell { x: 0, y: 0 });
        assert_eq!(apply(&mut picker, Key::Enter), Action::Redraw);
        assert_eq!(apply(&mut picker, Key::Enter), Action::Ignored, "already that colour");
    }

    #[test]
    fn backspace_and_delete_erase_the_cell_under_the_cursor() {
        let mut picker = picker(1, "ab");
        picker.set_focus(Focus::Swatch { at: 0 });
        press(&mut picker, &[Key::Enter]);
        picker.set_focus(Focus::Cell { x: 0, y: 0 });
        press(&mut picker, &[Key::Enter]);
        assert!(picker.canvas().cell(0, 0).unwrap().ink.is_some());

        assert_eq!(apply(&mut picker, Key::Backspace), Action::Redraw);
        assert_eq!(picker.canvas().cell(0, 0).unwrap().ink, None);
        assert_eq!(apply(&mut picker, Key::Del), Action::Ignored, "nothing left to erase");
        picker.set_focus(Focus::Add);
        assert_eq!(apply(&mut picker, Key::Backspace), Action::Ignored, "not on a cell");
    }

    #[test]
    fn ctrl_s_asks_for_a_save_rather_than_doing_one() {
        let mut picker = picker(1, "ab");
        assert_eq!(apply(&mut picker, Key::Char(CTRL_S)), Action::Save);
    }

    /// `RawMode` turns off signal generation, so Ctrl+C arrives as a keystroke and nothing else
    /// will ever act on it. A loop that ignores it is a loop the user cannot interrupt — and an
    /// interrupt that argued about unsaved work would not be an interrupt.
    #[test]
    fn ctrl_c_closes_at_once_even_with_unsaved_work() {
        let mut picker = picker(1, "ab");
        picker.set_focus(Focus::Swatch { at: 0 });
        press(&mut picker, &[Key::Enter]);
        picker.set_focus(Focus::Cell { x: 0, y: 0 });
        press(&mut picker, &[Key::Enter]);
        assert!(picker.is_dirty());
        assert_eq!(apply(&mut picker, Key::CtrlC), Action::Close);
    }

    #[test]
    fn esc_and_ctrl_x_close_a_clean_picker_at_once() {
        for key in [Key::Escape, Key::Char(CTRL_X)] {
            let mut picker = picker(1, "ab");
            let named = format!("{key:?}");
            assert_eq!(apply(&mut picker, key), Action::Close, "{named}");
        }
    }

    /// Work that has not reached the file is not thrown away on one keystroke: the first close
    /// warns, the second discards, and anything in between withdraws the warning.
    #[test]
    fn closing_with_unsaved_work_warns_first_and_any_other_key_withdraws_it() {
        let mut dirty = picker(1, "ab");
        dirty.set_focus(Focus::Swatch { at: 0 });
        press(&mut dirty, &[Key::Enter]);
        dirty.set_focus(Focus::Cell { x: 0, y: 0 });
        press(&mut dirty, &[Key::Enter]);

        assert_eq!(apply(&mut dirty, Key::Escape), Action::Redraw, "warned, not closed");
        assert!(dirty.notice().is_some_and(|n| n.contains("unsaved")), "{:?}", dirty.notice());
        assert_eq!(apply(&mut dirty, Key::Escape), Action::Close, "the second press goes through");

        // The same again, but with a key in between: the warning is withdrawn and the next
        // close request warns afresh.
        let mut again = picker(1, "ab");
        again.set_focus(Focus::Swatch { at: 0 });
        press(&mut again, &[Key::Enter]);
        again.set_focus(Focus::Cell { x: 0, y: 0 });
        press(&mut again, &[Key::Enter]);
        apply(&mut again, Key::Escape);
        assert_eq!(
            apply(&mut again, Key::Char('q')),
            Action::Redraw,
            "an otherwise-ignored key repaints, because it took the warning off the screen"
        );
        assert_eq!(again.notice(), None);
        assert_eq!(apply(&mut again, Key::Char(CTRL_X)), Action::Redraw, "warns again");
    }

    #[test]
    fn the_arrow_keys_move_the_cursor() {
        let mut picker = picker(2, "ab\ncd");
        assert_eq!(picker.focus(), Focus::Swatch { at: 0 });
        assert_eq!(apply(&mut picker, Key::ArrowDown), Action::Redraw);
        assert_eq!(picker.focus(), Focus::Swatch { at: 1 });
        apply(&mut picker, Key::ArrowDown);
        assert_eq!(picker.focus(), Focus::Add);
        apply(&mut picker, Key::ArrowDown);
        assert_eq!(picker.focus(), Focus::Cell { x: 0, y: 0 });
        assert_eq!(apply(&mut picker, Key::ArrowRight), Action::Redraw);
        assert_eq!(picker.focus(), Focus::Cell { x: 1, y: 0 });
        apply(&mut picker, Key::ArrowUp);
        assert_eq!(picker.focus(), Focus::Add, "and back out of the art");
    }

    #[test]
    fn tab_and_shift_tab_are_down_and_up() {
        let mut picker = picker(2, "ab");
        apply(&mut picker, Key::Tab);
        assert_eq!(picker.focus(), Focus::Swatch { at: 1 });
        apply(&mut picker, Key::BackTab);
        assert_eq!(picker.focus(), Focus::Swatch { at: 0 });
    }

    /// An edge refuses the move, and refusing is not a reason to redraw an identical frame.
    #[test]
    fn a_key_that_moves_nothing_is_ignored_rather_than_redrawn() {
        let mut picker = picker(1, "ab");
        assert_eq!(apply(&mut picker, Key::ArrowUp), Action::Ignored, "top edge");
        assert_eq!(apply(&mut picker, Key::ArrowLeft), Action::Ignored, "one column");
        assert_eq!(picker.focus(), Focus::Swatch { at: 0 });
    }

    /// Terminals emit more than keystrokes, and every junk event that redraws is a wasted frame.
    #[test]
    fn a_key_with_no_meaning_here_is_ignored() {
        let mut picker = picker(1, "ab");
        picker.set_focus(Focus::Add);
        for key in [Key::Char('q'), Key::PageUp, Key::Insert, Key::Unknown, Key::Home] {
            let named = format!("{key:?}");
            assert_eq!(apply(&mut picker, key), Action::Ignored, "{named}");
        }
    }

    // ---- undo, redo, and the file --------------------------------------------------------------

    #[test]
    fn undo_takes_back_a_paint_and_redo_puts_it_again() {
        let mut picker = picker(1, "ab");
        let ink = picker.palette().at(0).unwrap().color();
        picker.set_focus(Focus::Swatch { at: 0 });
        press(&mut picker, &[Key::Enter]);
        picker.set_focus(Focus::Cell { x: 0, y: 0 });
        press(&mut picker, &[Key::Enter]);

        assert_eq!(apply(&mut picker, Key::Char(CTRL_Z)), Action::Redraw);
        assert_eq!(picker.canvas().cell(0, 0).unwrap().ink, None);
        assert_eq!(apply(&mut picker, Key::Char(CTRL_Z)), Action::Ignored, "nothing older");
        assert_eq!(apply(&mut picker, Key::Char(CTRL_Y)), Action::Redraw);
        assert_eq!(picker.canvas().cell(0, 0).unwrap().ink, Some(ink));
        assert_eq!(apply(&mut picker, Key::Char(CTRL_Y)), Action::Ignored, "nothing newer");
    }

    /// A new edit after an undo starts a new future: what was undone can no longer be redone.
    #[test]
    fn a_new_edit_after_undo_discards_the_redo_stack() {
        let mut picker = picker(2, "ab");
        picker.set_focus(Focus::Swatch { at: 0 });
        press(&mut picker, &[Key::Enter]);
        picker.set_focus(Focus::Cell { x: 0, y: 0 });
        press(&mut picker, &[Key::Enter]);
        press(&mut picker, &[Key::Char(CTRL_Z)]);
        picker.set_focus(Focus::Cell { x: 1, y: 0 });
        press(&mut picker, &[Key::Enter]);
        assert_eq!(apply(&mut picker, Key::Char(CTRL_Y)), Action::Ignored);
    }

    /// Undoing the button removes the swatch it added — and if that swatch was the brush, the
    /// brush is gone with it rather than pointing at nothing.
    #[test]
    fn undoing_an_added_swatch_removes_it_and_drops_it_as_the_brush() {
        let mut picker = picker(0, "ab");
        press(&mut picker, &[Key::Enter]); // [+]
        picker.set_focus(Focus::Swatch { at: 0 });
        press(&mut picker, &[Key::Enter]); // pick it
        assert_eq!(picker.brush(), Some("colour 1"));

        assert!(picker.undo());
        assert!(picker.palette().is_empty());
        assert_eq!(picker.brush(), None);
        assert_eq!(picker.focus(), Focus::Add, "the cursor was on a row that no longer exists");
        assert!(picker.redo());
        assert_eq!(picker.palette().len(), 1, "and it comes back");
    }

    /// The trap in a naive save point: undo to it, edit again, and the history is the same
    /// LENGTH as when saved with different CONTENT. That must still read as dirty.
    #[test]
    fn dirtiness_tracks_the_saved_point_and_not_merely_the_edit_count() {
        let mut picker = picker(2, "ab");
        assert!(!picker.is_dirty(), "fresh");
        picker.set_focus(Focus::Swatch { at: 0 });
        press(&mut picker, &[Key::Enter]);
        picker.set_focus(Focus::Cell { x: 0, y: 0 });
        press(&mut picker, &[Key::Enter]);
        assert!(picker.is_dirty());

        picker.history.mark_saved();
        assert!(!picker.is_dirty(), "saved");
        press(&mut picker, &[Key::Char(CTRL_Z)]);
        assert!(picker.is_dirty(), "one behind the save");
        press(&mut picker, &[Key::Char(CTRL_Y)]);
        assert!(!picker.is_dirty(), "back on it");

        press(&mut picker, &[Key::Char(CTRL_Z)]);
        picker.set_focus(Focus::Cell { x: 1, y: 0 });
        press(&mut picker, &[Key::Enter]); // same length as when saved, different content
        assert!(picker.is_dirty(), "the save point is no longer reachable");
    }

    #[test]
    fn a_picker_built_in_code_has_nowhere_to_save() {
        let mut picker = picker(1, "ab");
        assert!(picker.save().is_err());
    }

    /// The whole feature through the file: paint, save, reopen, and find it as it was left.
    #[test]
    fn what_is_saved_is_what_is_reopened() {
        let dir = std::env::temp_dir().join(format!("arterminal-ui-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("art.txt");
        std::fs::write(&path, "ab\ncd\n").expect("write");

        let mut picker = Picker::open(&path).expect("plain text opens");
        assert!(picker.palette().is_empty(), "no palette data in a plain file");
        assert_eq!(picker.path(), Some(path.as_path()));
        press(&mut picker, &[Key::Enter]); // add a colour
        picker.set_focus(Focus::Swatch { at: 0 });
        press(&mut picker, &[Key::Enter]); // pick it
        picker.set_focus(Focus::Cell { x: 1, y: 1 });
        press(&mut picker, &[Key::Enter]); // paint d
        let ink = picker.palette().at(0).unwrap().color();
        assert!(picker.is_dirty());

        let saved = picker.save().expect("writes");
        assert_eq!(saved, path);
        assert!(!picker.is_dirty());

        let again = Picker::open(&path).expect("what we wrote reads back");
        assert_eq!(again.canvas().cell(1, 1).unwrap().ink, Some(ink));
        assert_eq!(again.palette().get("colour 1").map(Swatch::color), Some(ink));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_that_cannot_be_read_names_itself() {
        let err = Picker::open("/nonexistent/nowhere.txt").expect_err("no such file");
        assert!(err.to_string().starts_with("/nonexistent/nowhere.txt"), "{err}");
    }

    // ---- recolouring --------------------------------------------------------------------------

    /// A canvas cell holds a colour, not a reference — so moving a swatch has to rewrite the
    /// pixels, and this is the operation that does it. Without this, a palette and a drawing would
    /// simply drift apart.
    #[test]
    fn moving_a_swatch_repaints_every_cell_that_was_using_it() {
        let mut palette = Palette::new();
        palette.push("ember", RED).expect("fresh");
        palette.push("sky", BLUE).expect("distinct");
        let mut picker =
            Picker::new(Canvas::from_text("abc").expect("valid")).with_palette(palette);
        picker.canvas_mut().cell_mut(0, 0).unwrap().ink = Some(RED);
        picker.canvas_mut().cell_mut(1, 0).unwrap().ink = Some(BLUE);
        // Cell 2 stays uninked, so "rewrite the matching cells" must not mean "rewrite all".

        let moved = Rgb::new(20, 200, 40);
        picker.set_swatch_color("ember", moved).expect("that colour is free");

        assert_eq!(picker.canvas().cell(0, 0).unwrap().ink, Some(moved), "the cell followed");
        assert_eq!(picker.canvas().cell(1, 0).unwrap().ink, Some(BLUE), "its neighbour did not");
        assert_eq!(picker.canvas().cell(2, 0).unwrap().ink, None, "and an uninked cell is left be");
    }

    /// Cells belonging to a FOLLOWER move too, without the caller naming it — which is the whole
    /// point of a derived swatch, and the reason the palette hands back every pair rather than
    /// just the one it was asked about.
    #[test]
    fn moving_a_base_repaints_the_cells_of_what_follows_it() {
        let darker = HsbOffset { brightness: -60, ..HsbOffset::default() };
        let mut palette = Palette::new();
        palette.push("base", RED).expect("fresh");
        palette.push_derived("shade", "base", darker).expect("base exists");
        let shade = palette.get("shade").expect("just pushed").color();

        let mut picker = Picker::new(Canvas::from_text("ab").expect("valid")).with_palette(palette);
        picker.canvas_mut().cell_mut(0, 0).unwrap().ink = Some(RED);
        picker.canvas_mut().cell_mut(1, 0).unwrap().ink = Some(shade);

        let moved = Rgb::new(20, 200, 40);
        picker.set_swatch_color("base", moved).expect("free");

        let now_shade = picker.palette().get("shade").expect("still here").color();
        assert_ne!(now_shade, shade, "the follower moved");
        assert_eq!(picker.canvas().cell(0, 0).unwrap().ink, Some(moved));
        assert_eq!(
            picker.canvas().cell(1, 0).unwrap().ink,
            Some(now_shade),
            "and its cells followed it, though nobody named that swatch"
        );
    }

    /// Atomic across BOTH halves. The palette refuses the move whole; the canvas must never have
    /// been touched either, or a rejected edit would still have half-repainted the drawing.
    #[test]
    fn a_refused_move_leaves_the_drawing_exactly_as_it_was() {
        let mut palette = Palette::new();
        palette.push("ember", RED).expect("fresh");
        palette.push("sky", BLUE).expect("distinct");
        let mut picker = Picker::new(Canvas::from_text("ab").expect("valid")).with_palette(palette);
        picker.canvas_mut().cell_mut(0, 0).unwrap().ink = Some(RED);
        picker.canvas_mut().cell_mut(1, 0).unwrap().ink = Some(BLUE);
        let before = picker.canvas().clone();

        let refused = picker.set_swatch_color("ember", BLUE);
        assert!(refused.is_err(), "sky is taken");
        assert_eq!(picker.canvas(), &before, "not one pixel moved");
        assert_eq!(picker.palette().get("ember").unwrap().color(), RED, "nor the swatch");
    }

    /// The interaction that would otherwise put an ownerless colour back on the canvas: paint,
    /// recolour the swatch, then undo the paint. The undo must restore the swatch's colour AS IT
    /// NOW IS — the history was carried along with the move.
    #[test]
    fn undo_after_a_recolour_restores_the_colour_the_swatch_now_has() {
        let mut palette = Palette::new();
        palette.push("ember", RED).expect("fresh");
        let mut picker = Picker::new(Canvas::from_text("ab").expect("valid")).with_palette(palette);
        picker.canvas_mut().cell_mut(0, 0).unwrap().ink = Some(RED); // painted before the session
        picker.set_focus(Focus::Swatch { at: 0 });
        press(&mut picker, &[Key::Enter]);
        picker.set_focus(Focus::Cell { x: 1, y: 0 });
        press(&mut picker, &[Key::Enter]); // paint b red, recorded as None -> RED

        let moved = Rgb::new(20, 200, 40);
        picker.set_swatch_color("ember", moved).expect("free");
        assert_eq!(picker.canvas().cell(1, 0).unwrap().ink, Some(moved));

        assert!(picker.undo(), "take back the paint of b");
        assert_eq!(picker.canvas().cell(1, 0).unwrap().ink, None);
        assert!(picker.redo(), "and put it again");
        assert_eq!(
            picker.canvas().cell(1, 0).unwrap().ink,
            Some(moved),
            "in the colour the swatch has NOW, not the one it had when painted"
        );
        assert!(
            picker.canvas().rows().flatten().all(|c| c.ink.is_none_or(|i| i == moved)),
            "no pixel holds a colour no swatch owns"
        );
    }

    /// The whole feature, end to end through the public surface: press the button, and the frame
    /// grows a row showing the colour that was added.
    #[test]
    fn adding_a_colour_grows_the_palette_and_the_frame_that_shows_it() {
        with_colour();
        let mut picker = picker(0, "ab");
        assert_eq!(picker.focus(), Focus::Add, "with no swatches the button is the top row");

        let before = render(&picker, 200, 60).len();
        apply(&mut picker, Key::Enter);
        let after = render(&picker, 200, 60);

        assert_eq!(after.len(), before + 1, "one new row");
        let swatch = picker.palette().at(0).expect("just added");
        let colour = swatch.color();
        assert!(
            after[0].contains(&format!("\x1b[48;2;{};{};{}m", colour.r, colour.g, colour.b)),
            "drawn in the colour it was given: {:?}",
            after[0]
        );
        // A GENERATED name, not the hex. A label is a durable handle now — a derived swatch names
        // its base by label — so naming a swatch after a colour it is free to leave would either
        // go stale or, if kept in step, break every derivation pointing at it.
        assert_eq!(swatch.label(), "colour 1");
        assert!(after[0].contains("# colour 1"), "labelled by name: {:?}", after[0]);
        assert!(
            !after[0].contains(&colour.to_string()),
            "and NOT by its hex, which it is free to stop matching: {:?}",
            after[0]
        );
    }
}
