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
//! it, and a run of canvas cells folds ink and inversion into a single style applied once to
//! their glyphs — plain characters, since the canvas refuses control characters, and so unable
//! to contain a reset.
//!
//! THE CHANGE THAT WOULD BREAK THIS is the first feature needing to highlight a MULTI-CELL RUN: a
//! selection rectangle, a focused swatch shown inverted rather than gutter-marked, a status line
//! embedding a swatch preview. The tempting implementation is to wrap the already-rendered run,
//! and that is exactly the bug above arriving here. Re-render the run's cells instead, with the
//! inversion composed into each cell's style. It costs more allocations and it cannot go wrong.

use crate::canvas::{Canvas, Cell, LoadCause, LoadError};
use crate::color::{Ink, Rgb, Rng};
use crate::cursor::{Dir, Focus};
use crate::dial::{Channel, Dial};
use crate::document;
use crate::keys::{KeyCode, KeyEvent, KeyKind};
use crate::palette::{Palette, PaletteError, Recolour, Swatch};
use crate::{input, paint};
use console::Term;
use std::borrow::Cow;
use std::collections::VecDeque;
use std::ffi::OsString;
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
    /// Whether moving the cursor over a cell paints it, erases it, or leaves it alone.
    pen: Pen,
    /// What keeps the pen down, while it is. `None` exactly when [`Picker::pen`] is up.
    grip: Option<Grip>,
    /// How many cells a side the pen covers: 1 is the cursor's cell alone. `[` and `]` step it.
    pen_size: usize,
    /// Which part of the picture is on screen, when the terminal is too small for all of it.
    view: View,
    /// A label being typed, while one is.
    editing: Option<LabelEdit>,
    /// A colour being dialled, while one is — for `[+]`'s new swatch, or F2's. See
    /// [`crate::dial`].
    colour_edit: Option<ColourEdit>,
    /// Whether the terminal reports key RELEASES, so a pen can be held rather than toggled. Set by
    /// [`run`] from what the terminal answered; `false` for a picker driven any other way. Only
    /// the hint reads it — [`apply`] needs no mode, see there.
    hold_keys: bool,
    /// Whether the terminal reports Shift on a Ctrl chord, so Ctrl+Shift+Z is not Ctrl+Z. Set by
    /// [`run`]; only the hint reads it, to name the redo key this terminal can actually send.
    shift_on_ctrl: bool,
    /// Whether the terminal draws East Asian Ambiguous characters — `·`, `—`, `…`, block
    /// elements — two columns wide. Set by [`run`], which asks the terminal; [`render`] measures
    /// and clips every line by it, so no line is ever wider than the terminal draws it.
    wide_ambiguous: bool,
    /// Whether the art is shown split — a solid-colour canvas to paint on, and the coloured art
    /// beside or under it as a preview — rather than as one picture. F6 and Shift+F6 choose.
    split: Option<Split>,
    /// Whether the split view's preview shows the cursor too. `c` flips it: the marker covers the
    /// glyph under it, and sometimes that glyph is the thing being judged.
    art_cursor: bool,
    history: History,
    /// One line of news for the status row — a save that succeeded or did not, a warning before
    /// discarding work. Shown once and cleared by the next keystroke.
    notice: Option<String>,
    /// Set by a close request that found unsaved work; the next close request goes through, any
    /// other key clears it.
    quit_armed: bool,
    /// Where a save goes. Set by [`Picker::open`], absent for a picker built in code.
    path: Option<PathBuf>,
    /// Past this many bytes in all, a red line under the hints says where the memory goes. See
    /// [`MEMORY_WARNING_ABOVE`].
    memory_warning_above: usize,
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
            pen: Pen::Up,
            grip: None,
            pen_size: 1,
            view: View::default(),
            editing: None,
            colour_edit: None,
            hold_keys: false,
            shift_on_ctrl: false,
            wide_ambiguous: false,
            split: None,
            art_cursor: true,
            history: History::default(),
            notice: None,
            quit_armed: false,
            path: None,
            memory_warning_above: MEMORY_WARNING_ABOVE,
        }
    }

    /// Read a document from `path` — art, colours and palette, as [`crate::document`] lays them
    /// out — and remember the path so [`Picker::save`] knows where to go.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, LoadError> {
        let path = path.as_ref();
        Self::from_text(path, &document::read(path)?)
    }

    /// [`Picker::open`] for a file already read — by [`document::read`], say, so that it could
    /// be [measured](document::measure) first. `path` is where it came from, and where a save
    /// will go.
    pub fn from_text(path: impl AsRef<Path>, text: &str) -> Result<Self, LoadError> {
        let path = path.as_ref();
        let at = |source| LoadError { path: path.to_path_buf(), source };
        let doc = document::parse(text).map_err(|err| at(LoadCause::Document(err)))?;
        let mut picker = Self::new(doc.canvas).with_palette(doc.palette);
        picker.path = Some(path.to_path_buf());
        Ok(picker)
    }

    /// Start from an existing palette rather than an empty one.
    pub fn with_palette(mut self, palette: Palette) -> Self {
        self.palette = palette;
        self.view.follow = true;
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
            self.view.follow = true;
            self.focus = focus;
        }
        exists
    }

    /// The label of the swatch that paints, once one has been picked.
    pub fn brush(&self) -> Option<&str> {
        self.brush.as_deref()
    }

    pub fn pen(&self) -> Pen {
        self.pen
    }

    /// Where the memory this picker holds goes: the drawing, and everything undo and redo can
    /// reach. Both as allocated, not as used — what the machine is really paying.
    pub fn memory(&self) -> Memory {
        Memory { image: self.canvas.bytes(), history: self.history.bytes() }
    }

    /// Past how many bytes in all the red memory line appears — [`MEMORY_WARNING_ABOVE`] unless
    /// a caller that knows its machine says otherwise. Nothing is ever capped; the line informs.
    pub fn set_memory_warning_above(&mut self, bytes: usize) {
        self.memory_warning_above = bytes;
    }

    fn warns_of_memory(&self) -> bool {
        self.memory().total() > self.memory_warning_above
    }

    /// The red line under the hints, once memory passes the line: how much is the image and how
    /// much the undo history, since those are the two things a person can do something about.
    fn memory_warning(&self) -> Option<String> {
        let memory = self.memory();
        self.warns_of_memory().then(|| {
            format!(
                "memory {}: the image {}, the undo history {}; reopening the file starts a fresh \
                 history",
                readable_bytes(memory.total()),
                readable_bytes(memory.image),
                readable_bytes(memory.history),
            )
        })
    }

    /// How many cells a side the pen covers — see [`Picker::grow_pen`].
    pub fn pen_size(&self) -> usize {
        self.pen_size
    }

    /// `]`: a pen one cell bigger each way — 1×1, 2×2, 3×3 — up to the canvas's longer side,
    /// which is what a square centred anywhere needs to be able to cover the whole drawing. A pen
    /// that is down covers its new size at once. `false` when it is already as big as it gets.
    pub fn grow_pen(&mut self) -> bool {
        let largest = self.canvas.width().max(self.canvas.height());
        self.resize_pen(self.pen_size + 1, largest)
    }

    /// `[`: a pen one cell smaller each way, down to the cursor's cell alone. `false` at 1×1.
    pub fn shrink_pen(&mut self) -> bool {
        self.resize_pen(self.pen_size.saturating_sub(1), usize::MAX)
    }

    fn resize_pen(&mut self, to: usize, largest: usize) -> bool {
        if to == 0 || to > largest || to == self.pen_size {
            return false;
        }
        self.pen_size = to;
        self.apply_pen();
        true
    }

    pub fn is_split(&self) -> bool {
        self.split.is_some()
    }

    /// How the art is split into a canvas and a preview, if it is.
    pub fn split(&self) -> Option<Split> {
        self.split
    }

    /// Say whether the terminal delivers key releases — normally [`run`]'s to know, public for a
    /// caller running its own loop that has found out for itself.
    pub fn set_hold_keys(&mut self, hold_keys: bool) {
        self.hold_keys = hold_keys;
    }

    /// Say whether the terminal reports Shift on a Ctrl chord — normally [`run`]'s to know. Only
    /// the hint changes: Ctrl+Shift+Z redoes whenever it arrives, and Ctrl+Y always does.
    pub fn set_shift_on_ctrl(&mut self, shift_on_ctrl: bool) {
        self.shift_on_ctrl = shift_on_ctrl;
    }

    /// Say whether the terminal draws East Asian Ambiguous characters two columns wide —
    /// normally [`run`]'s to find out, by asking the terminal.
    ///
    /// This changes only how [`render`] MEASURES, never which glyphs a canvas accepts: that rule
    /// is the file's, one cell per glyph by the narrow width, so a drawing loads the same on every
    /// machine. On a terminal set wide, a drawing that uses such glyphs looks sheared — each takes
    /// two columns there, which nothing here can change — but the frame is clipped to what the
    /// terminal really draws, so it never wraps and never loses its place.
    pub fn set_wide_ambiguous(&mut self, wide: bool) {
        self.wide_ambiguous = wide;
    }

    /// The label being typed, if a swatch is being renamed right now.
    pub fn editing(&self) -> Option<&str> {
        self.editing.as_ref().map(|edit| edit.text.as_str())
    }

    /// The colour being dialled, if one is right now — see [`Picker::begin_new_colour`] and
    /// [`Picker::begin_recolour`].
    pub fn dial(&self) -> Option<&Dial> {
        self.colour_edit.as_ref().map(|edit| &edit.dial)
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

    /// Paint the cells under the pen with the brush: the cursor's cell, or the square around it
    /// when the pen is bigger — see [`Picker::grow_pen`]. `false` — and no edit recorded — when
    /// the cursor is not on a cell, there is no brush, or every cell already holds that colour.
    pub fn paint(&mut self) -> bool {
        let Some(color) = self.brush_color() else { return false };
        self.ink_footprint(Some(color))
    }

    /// Clear the cells under the pen back to the terminal's own colour.
    pub fn erase(&mut self) -> bool {
        self.ink_footprint(None)
    }

    /// `i`: make the colour under the cursor the brush — what going up to its swatch and picking
    /// it would do, without leaving the canvas. `false` when that is already the brush, or there
    /// is nothing to pick up, which a notice then says.
    pub fn pick_colour(&mut self) -> bool {
        let Focus::Cell { x, y } = self.focus else { return false };
        let Some(ink) = self.canvas.cell(x, y).and_then(|cell| cell.ink) else {
            self.notice = Some("nothing to pick up: this cell has no colour".to_string());
            return false;
        };
        // Every colour on the canvas has a swatch — the palette is built to guarantee it — so a
        // miss here is a broken invariant, reported rather than trusted to be impossible.
        let Some(label) = self.palette.holder_of(ink).map(|swatch| swatch.label().to_string())
        else {
            self.notice = Some("this colour has no swatch to pick".to_string());
            return false;
        };
        if self.brush.as_deref() == Some(&*label) {
            return false;
        }
        self.brush = Some(label);
        true
    }

    fn brush_color(&self) -> Option<Ink> {
        self.brush.as_deref().and_then(|b| self.palette.get(b)).map(Swatch::color)
    }

    /// Set every cell the pen covers to `to`, recording what each was. A tap with a big pen is
    /// still ONE edit: outside a stroke, the cells it changes are folded together here, exactly
    /// as a stroke's are when its pen lifts.
    fn ink_footprint(&mut self, to: Option<Ink>) -> bool {
        let Some(Footprint { xs, ys }) = self.footprint() else { return false };
        let own_stroke = !self.history.in_stroke();
        if own_stroke {
            self.history.begin_stroke();
        }
        let mut changed = false;
        for y in ys {
            for x in xs.clone() {
                let cell =
                    self.canvas.cell_mut(x, y).expect("the footprint is clipped to the canvas");
                let from = cell.ink;
                if from != to {
                    cell.ink = to;
                    self.history.push(Edit::Paint { x, y, from, to });
                    changed = true;
                }
            }
        }
        if own_stroke {
            self.history.end_stroke();
        }
        changed
    }

    /// A HOLD key went down on a cell — Space or Enter to paint, Backspace to erase.
    ///
    /// Where releases are reported (see [`Picker::set_hold_keys`]), the pen stays down while the
    /// key is held and every cell the cursor passes over takes it; the key's release lifts it.
    /// Where they are not, the press is a TAP: the one cell under the cursor, and the pen stays up.
    /// It never toggles — toggling has keys of its own, see [`Picker::toggle_pen`] — because a
    /// hold key that sometimes latched would be one whose meaning depended on the terminal.
    ///
    /// Several hold keys can be down at once — Space, then Backspace without letting go — and
    /// the last one pressed is in charge. When it comes back up, the pen is the one underneath's
    /// again, if that is still held; see [`Picker::release_key`].
    ///
    /// A second press while this same key is already holding the pen means its release was never
    /// delivered. Rather than trust releases that are going missing, the picker stops asking for
    /// holds and says so: from then on the key taps. A tmux that answers the protocol query but
    /// drops releases is the known case — see [`apply`].
    pub fn hold_pen(&mut self, pen: Pen, key: KeyCode) -> bool {
        if !matches!(self.focus, Focus::Cell { .. }) {
            return false;
        }
        if pen == Pen::Painting && self.brush_color().is_none() {
            return false;
        }
        // The heal is a change in itself — a lifted pen and a notice — even when the tap that
        // follows lands on a cell that already looks right.
        let healed = self.held_keys().contains(&key);
        if healed {
            self.lift_pen();
            self.hold_keys = false;
            self.notice = Some(
                "key releases are not arriving — holding now taps one cell; b and del still toggle"
                    .to_string(),
            );
        }
        if !self.hold_keys {
            let tapped = match pen {
                Pen::Painting => self.paint(),
                Pen::Erasing => self.erase(),
                Pen::Up => false,
            };
            return tapped || healed;
        }
        match &mut self.grip {
            // Another key is already holding the pen: this one goes on top, in charge until it
            // comes back up. Pressed, so it acts where the cursor stands, as any press does.
            Some(Grip::Held(stack)) => {
                stack.push((key, pen));
                if self.pen != pen {
                    self.switch_pen(pen);
                    self.apply_pen();
                }
            }
            _ => self.put_pen_down(pen, Grip::Held(vec![(key, pen)])),
        }
        true
    }

    /// A TOGGLE key went down on a cell — `b` for the painting pen, Delete for the erasing one.
    /// Down if it was up (or down some other way), up if this toggle already had it down.
    pub fn toggle_pen(&mut self, pen: Pen) -> bool {
        if !matches!(self.focus, Focus::Cell { .. }) {
            return false;
        }
        if self.pen == pen && self.grip == Some(Grip::Toggled) {
            self.lift_pen();
            return true;
        }
        if pen == Pen::Painting && self.brush_color().is_none() {
            return false;
        }
        self.put_pen_down(pen, Grip::Toggled);
        true
    }

    /// Whatever held the pen before gives way: one stroke ends, and a new one starts here.
    fn put_pen_down(&mut self, pen: Pen, grip: Grip) {
        self.lift_pen();
        self.pen = pen;
        self.grip = Some(grip);
        self.history.begin_stroke();
        self.apply_pen();
    }

    /// The pen changes what it does without lifting: one stroke ends and the next begins, so each
    /// can be undone on its own.
    fn switch_pen(&mut self, pen: Pen) {
        self.history.end_stroke();
        self.history.begin_stroke();
        self.pen = pen;
    }

    /// A key came back up. `false` if it was not holding the pen. Otherwise the pen lifts —
    /// unless another held key is still down, and then the pen is that key's again.
    pub fn release_key(&mut self, key: KeyCode) -> bool {
        let Some(Grip::Held(stack)) = &mut self.grip else { return false };
        let Some(at) = stack.iter().position(|&(held, _)| held == key) else { return false };
        stack.remove(at);
        match stack.last().map(|&(_, pen)| pen) {
            None => self.lift_pen(),
            // Handed back, not pressed: nothing is applied where the cursor stands, or letting go
            // of Backspace would repaint the very cell it had just erased.
            Some(pen) if pen != self.pen => self.switch_pen(pen),
            Some(_) => {}
        }
        true
    }

    /// The keys holding the pen down, in the order they went down — none when it is toggled or
    /// up.
    fn held_keys(&self) -> Vec<KeyCode> {
        match &self.grip {
            Some(Grip::Held(stack)) => stack.iter().map(|&(key, _)| key).collect(),
            _ => Vec::new(),
        }
    }

    /// Lift the pen, closing the stroke it was drawing. Harmless when it is already up.
    pub fn lift_pen(&mut self) {
        if self.pen != Pen::Up {
            self.pen = Pen::Up;
            self.grip = None;
            self.history.end_stroke();
        }
    }

    /// Whether the pen is being held down by a key rather than toggled.
    pub fn pen_is_held(&self) -> bool {
        matches!(self.grip, Some(Grip::Held(_)))
    }

    /// What a pen that is down does to the cell under the cursor.
    fn apply_pen(&mut self) -> bool {
        match self.pen {
            Pen::Up => false,
            Pen::Painting => self.paint(),
            Pen::Erasing => self.erase(),
        }
    }

    /// Start renaming the swatch `at` rows down: its label becomes the text being typed, with the
    /// cursor at its end so the name can be extended or backspaced.
    pub fn begin_rename(&mut self, at: usize) -> bool {
        self.begin_naming(at, false)
    }

    /// [`Picker::begin_rename`], saying whether the name `joins` the edit just recorded — see
    /// [`LabelEdit::joins`].
    fn begin_naming(&mut self, at: usize, joins: bool) -> bool {
        let Some(swatch) = self.palette.at(at) else { return false };
        let label = swatch.label().to_string();
        self.lift_pen();
        self.editing = Some(LabelEdit { at, text: label.clone(), original: label, joins });
        self.view.follow = true;
        self.focus = Focus::Swatch { at };
        true
    }

    /// Keep the label as typed. `Err` — and still editing — if the palette refuses the name,
    /// so the person can fix it rather than lose it.
    ///
    /// A name given straight after `[+]` added its swatch, or F2 kept its new colour, is ONE undo
    /// with that edit, as it was one thing to do: taking back half of it — the name, but not the
    /// colour it came with — is not what anyone did.
    pub fn commit_rename(&mut self) -> Result<(), PaletteError> {
        let Some(edit) = self.editing.as_ref() else { return Ok(()) };
        let (from, to, joins) = (edit.original.clone(), edit.text.clone(), edit.joins);
        if from != to {
            // The edit this name completes is held aside while the rename is carried into the
            // history, so that it keeps the name it was made under — see [`Edit::Stroke`].
            let first = if joins { self.history.take_done() } else { None };
            if let Err(why) = self.apply_rename(&from, &to) {
                // Refused before anything moved, so it goes back exactly as it was.
                if let Some(first) = first {
                    self.history.put_done(first);
                }
                return Err(why);
            }
            let rename = Edit::Rename { from, to };
            match first {
                Some(first) => self.history.put_done(Edit::Stroke(vec![first, rename])),
                None => self.history.push(rename),
            }
        }
        self.editing = None;
        Ok(())
    }

    /// Give up on the rename; the label is as it was.
    pub fn cancel_rename(&mut self) -> bool {
        self.editing.take().is_some()
    }

    /// Rename in the palette, and everywhere else the old name was held.
    fn apply_rename(&mut self, from: &str, to: &str) -> Result<(), PaletteError> {
        self.palette.rename(from, to)?;
        self.carry_name(from, to);
        Ok(())
    }

    /// Carry a swatch's new name to everything else that holds the old one — the brush, and the
    /// history — once the palette has taken it.
    fn carry_name(&mut self, from: &str, to: &str) {
        if self.brush.as_deref() == Some(from) {
            self.brush = Some(to.to_string());
        }
        self.history.rename(from, to);
    }

    /// `[+]`: start choosing the colour of a new swatch on the dial, from a random colour nothing
    /// holds — so Enter at once keeps one, and consecutive swatches start out apart. Nothing is
    /// added until the colour is kept: see [`Picker::commit_colour`].
    pub fn begin_new_colour(&mut self) {
        // With every colour a random draw can reach already held, any of them will do: the dial
        // can be turned on to a free one, which is the whole point of having it.
        let start = match self.palette.free_random_color(&mut self.rng) {
            Ok(free) => free,
            Err(_) => Rgb::random(&mut self.rng),
        };
        self.lift_pen();
        self.colour_edit = Some(ColourEdit { target: Target::New, dial: Dial::new(start) });
        self.view.follow = true;
    }

    /// F2: start choosing a new colour for the swatch `at` rows down, on the dial, from the colour
    /// it has. Nothing changes until the colour is kept: see [`Picker::commit_colour`]. `false`
    /// if there is no such swatch.
    pub fn begin_recolour(&mut self, at: usize) -> bool {
        let Some(swatch) = self.palette.at(at) else { return false };
        // A slot starts from the colour it usually is, since the dial turns colours of their own.
        // It stays a slot unless that colour is changed.
        let dial = Dial::new(swatch.color().approximate());
        self.lift_pen();
        self.colour_edit = Some(ColourEdit { target: Target::Existing(at), dial });
        self.view.follow = true;
        self.focus = Focus::Swatch { at };
        true
    }

    /// Keep the colour on the dial, and go on to naming its swatch, as [`Picker::begin_rename`]
    /// does: `[+]`'s colour becomes a new swatch under a generated name, and F2's becomes its
    /// swatch's colour — the drawing with it, see [`Picker::set_swatch_color`] — when it is a
    /// different colour at all. A dial turned and turned back is not, and records nothing. The
    /// name given next is one undo with what this records; see [`Picker::commit_rename`].
    ///
    /// `Err`, and still dialling, when another swatch holds the colour: two swatches cannot share
    /// one, and turning on to a free colour is better than losing the dial. What is being typed
    /// counts only once ended, which the keys do first — see [`Dial::end_typing`].
    pub fn commit_colour(&mut self) -> Result<(), PaletteError> {
        let Some(ColourEdit { target, dial }) = self.colour_edit else { return Ok(()) };
        let at = match target {
            Target::New => {
                let label = self.palette.push_unnamed(dial.rgb())?;
                self.history.push(Edit::AddSwatch { label, color: dial.rgb().into() });
                self.palette.len() - 1
            }
            Target::Existing(at) => {
                if dial.is_changed() {
                    self.recolour(at, dial.rgb())?;
                }
                at
            }
        };
        self.colour_edit = None;
        // Named next; the name is one undo with what was just recorded, if anything was.
        let recorded = target == Target::New || dial.is_changed();
        self.begin_naming(at, recorded);
        Ok(())
    }

    /// Give up on the colour: `[+]` adds nothing, and F2's swatch keeps the colour it had.
    pub fn cancel_colour(&mut self) -> bool {
        self.colour_edit.take().is_some()
    }

    /// Move the swatch `at` rows down to `to`, recorded so it can be undone.
    fn recolour(&mut self, at: usize, to: Rgb) -> Result<(), PaletteError> {
        let Some(swatch) = self.palette.at(at) else {
            return Err(PaletteError::UnknownLabel { label: format!("row {at}") });
        };
        let (label, from, to) = (swatch.label().to_string(), swatch.color(), Ink::Rgb(to));
        self.set_swatch_color(&label, to)?;
        // The dial turns colours of their own, so a slot swatch stops being one — said out loud,
        // because the swatch then looks the same everywhere and no longer follows the terminal's
        // palette, which nothing on screen would otherwise tell.
        if let Ink::Slot(slot) = from {
            self.notice = Some(format!(
                "{label:?} is a colour of its own now, no longer the terminal's slot {slot}"
            ));
        }
        self.history.push(Edit::Recolour { label, from, to });
        Ok(())
    }

    /// What keeping the colour on `edit`'s dial would do: every `(was, now)` pair it would make —
    /// none for `[+]`'s, which nothing holds yet, and none for F2's unchanged — or why it would be
    /// refused. Asked of the palette's own rules, so a frame says what Enter will do; see
    /// [`Palette::color_change`].
    fn dial_outcome(&self, edit: &ColourEdit) -> Result<Vec<(Ink, Ink)>, PaletteError> {
        let colour = edit.dial.rgb();
        match edit.target {
            Target::New => match self.palette.holder_of(colour) {
                Some(holder) => Err(PaletteError::DuplicateColor {
                    color: colour.into(),
                    held_by: holder.label().to_string(),
                }),
                None => Ok(Vec::new()),
            },
            Target::Existing(_) if !edit.dial.is_changed() => Ok(Vec::new()),
            Target::Existing(at) => {
                let label = self.palette.at(at).map_or("", Swatch::label);
                self.palette.color_change(label, colour).map(|change| change.changes().to_vec())
            }
        }
    }

    /// Take back the last edit. `false` when there is none — or when it could not be taken back,
    /// which a notice then explains: undoing a recolour can land on a colour some other swatch has
    /// since taken, and undoing a rename on a name since reused.
    pub fn undo(&mut self) -> bool {
        self.lift_pen();
        let Some(edit) = self.history.take_done() else { return false };
        match self.replay(&edit, Way::Back) {
            Ok(()) => {
                self.history.put_undone(edit);
                true
            }
            Err(why) => {
                self.history.put_done(edit);
                self.notice = Some(format!("cannot undo: {why}"));
                false
            }
        }
    }

    /// Put back the last edit taken back. `false` when there is none, or it no longer can be.
    pub fn redo(&mut self) -> bool {
        self.lift_pen();
        let Some(edit) = self.history.take_undone() else { return false };
        match self.replay(&edit, Way::Forward) {
            Ok(()) => {
                self.history.put_done(edit);
                true
            }
            Err(why) => {
                self.history.put_undone(edit);
                self.notice = Some(format!("cannot redo: {why}"));
                false
            }
        }
    }

    /// Replay `edit` one `way`, WHOLE or not at all.
    ///
    /// Only the palette can refuse — a colour, or a name, that some other swatch has since taken —
    /// and a stroke can hold more than one edit it might refuse: a swatch's new colour and its
    /// name. So it is asked first, of every part in the order the replay runs them, on a copy. A
    /// refusal there changes nothing at all; and once the copy has said yes, the palette itself
    /// cannot say no, since it is asked the same things in the same order from the same place.
    ///
    /// NOT the alternative of replaying part by part and walking back on a refusal: the walking
    /// back would be asked of the palette too, and could be refused in its turn. Before this, a
    /// stroke replayed part by part and stopped at a refusal with the parts before it done, while
    /// the history went on saying none of it was — harmless only as long as strokes held nothing
    /// but paint, which is never refused.
    fn replay(&mut self, edit: &Edit, way: Way) -> Result<(), PaletteError> {
        if edit.moves_the_palette() {
            let mut trial = self.palette.clone();
            edit.for_each_part(way, &mut |part| part.apply_to_palette(&mut trial, way).map(drop))?;
        }
        edit.for_each_part(way, &mut |part| self.replay_part(part, way))
            .expect("the palette agreed to every part a moment ago, on a copy, in this order");
        Ok(())
    }

    /// Replay one edit — not a stroke, whose parts come here one at a time — one `way`: the
    /// palette's share, then everything that follows from it.
    fn replay_part(&mut self, part: &Edit, way: Way) -> Result<(), PaletteError> {
        if let (Edit::AddSwatch { label, .. }, Way::Back) = (part, way) {
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
        }
        let moved = part.apply_to_palette(&mut self.palette, way)?;
        match (part, way) {
            (Edit::Paint { x, y, from, to }, way) => {
                let ink = match way {
                    Way::Back => *from,
                    Way::Forward => *to,
                };
                self.canvas.cell_mut(*x, *y).expect("the canvas does not resize").ink = ink;
            }
            (Edit::AddSwatch { label, .. }, Way::Back) => {
                if self.brush.as_deref() == Some(label) {
                    self.brush = None;
                }
                if !self.focus.exists(&self.palette, &self.canvas) {
                    self.view.follow = true;
                    self.focus = Focus::Add;
                }
            }
            (Edit::Recolour { .. }, _) => self.carry_colours(&moved),
            (Edit::Rename { from, to }, Way::Forward) => self.carry_name(from, to),
            (Edit::Rename { from, to }, Way::Back) => self.carry_name(to, from),
            (Edit::AddSwatch { .. }, Way::Forward) | (Edit::Stroke(_), _) => {}
        }
        Ok(())
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
    pub fn set_swatch_color(
        &mut self,
        label: &str,
        color: impl Into<Ink>,
    ) -> Result<Recolour, PaletteError> {
        let recolour = self.palette.set_color(label, color)?;
        self.carry_colours(recolour.changes());
        Ok(recolour)
    }

    /// Carry the palette's `(was, now)` colour changes into the drawing and the history — the
    /// half of a recolour the palette cannot do itself. See [`Picker::set_swatch_color`].
    fn carry_colours(&mut self, changes: &[(Ink, Ink)]) {
        for (was, now) in changes {
            for cell in self.canvas.cells_mut() {
                if cell.ink == Some(*was) {
                    cell.ink = Some(*now);
                }
            }
            self.history.recolour(*was, *now);
        }
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
        self.lift_pen();
        let path = path.as_ref();
        std::fs::write(path, document::render(&self.canvas, &self.palette))?;
        self.path = Some(path.to_path_buf());
        self.history.mark_saved();
        Ok(())
    }
}

impl Picker {
    /// Where an interrupted session's unsaved work goes: the document's own path with
    /// `.arterminal.tmp` appended — appended, so `art.txt` salvages to `art.txt.arterminal.tmp`
    /// and nothing about the original name is lost or guessed at. `None` for a picker that was
    /// never opened from a file.
    pub fn salvage_path(&self) -> Option<PathBuf> {
        let mut path: OsString = self.path.clone()?.into();
        path.push(SALVAGE_SUFFIX);
        Some(PathBuf::from(path))
    }

    /// Write unsaved work to [`Picker::salvage_path`], if there is any and anywhere to put it.
    /// Says where it went. Does NOT move the save point: the real file is still behind.
    pub fn salvage(&mut self) -> std::io::Result<Option<PathBuf>> {
        self.lift_pen();
        let Some(path) = self.salvage_path() else { return Ok(None) };
        if !self.is_dirty() {
            return Ok(None);
        }
        std::fs::write(&path, document::render(&self.canvas, &self.palette))?;
        Ok(Some(path))
    }
}

impl Picker {
    /// Lines the body has — everything [`render`] draws above the footer. See [`BodyLine`].
    fn body_len(&self) -> usize {
        let art = self.canvas.height();
        // `first_art_line` counts the dial's lines, while it is open.
        match self.split {
            Some(Split::Stacked) => self.first_art_line() + art + 1 + art + 1,
            _ => self.first_art_line() + art + 1,
        }
    }

    /// Where the art's first row is in the body: after the palette, `[+]`, the gap and the band
    /// of border above the art — and the dial's lines, while it is open.
    fn first_art_line(&self) -> usize {
        self.palette.len() + 3 + self.dial_lines().map_or(0, |(_, added)| added)
    }

    /// Where the dial's lines start in the body while it is open, and how many lines they add to
    /// it: all but one for F2's, which stand where its swatch's row was, and all of them for
    /// `[+]`'s, which stand before the button, where the new swatch will be.
    fn dial_lines(&self) -> Option<(usize, usize)> {
        self.colour_edit.as_ref().map(|edit| match edit.target {
            Target::Existing(at) => (at, DialLine::ALL.len() - 1),
            Target::New => (self.palette.len(), DialLine::ALL.len()),
        })
    }

    /// What the body's line `at` is. THE ONE PLACE the body's order is written down, so the
    /// renderer, the scrolling and the status row cannot disagree about where anything is.
    fn body_line(&self, at: usize) -> BodyLine {
        // The dial's lines, while it is open, push everything after them down: below here, `at`
        // is where the line would be without them.
        let dial = self.dial_lines();
        let at = match dial {
            Some((first, _)) if (first..first + DialLine::ALL.len()).contains(&at) => {
                return BodyLine::Dial(DialLine::ALL[at - first]);
            }
            Some((first, added)) if at >= first + DialLine::ALL.len() => at - added,
            _ => at,
        };
        let added = dial.map_or(0, |(_, added)| added);
        let (palette, art, first) =
            (self.palette.len(), self.canvas.height(), self.first_art_line() - added);
        let preview = first + art + 1;
        match at {
            at if at < palette => BodyLine::Swatch(at),
            at if at == palette => BodyLine::Add,
            at if at == palette + 1 => BodyLine::Gap,
            at if at >= first && at < first + art => BodyLine::Art(at - first),
            at if self.split == Some(Split::Stacked) && (preview..preview + art).contains(&at) => {
                BodyLine::Preview(at - preview)
            }
            _ => BodyLine::Border,
        }
    }

    /// The body line the cursor is on: the dial's middle one, while it is open.
    fn cursor_line(&self) -> usize {
        if let Some((first, _)) = self.dial_lines() {
            return first + 1;
        }
        match self.focus {
            Focus::Swatch { at } => at,
            Focus::Add => self.palette.len(),
            Focus::Cell { y, .. } => self.first_art_line() + y,
        }
    }

    /// The cells the pen covers: a square `pen_size` a side around the cursor, clipped to the
    /// canvas — `None` off it. Centred where it can be; an even size cannot be, and leans right
    /// and down, so the cursor stays the top-left of the middle four.
    fn footprint(&self) -> Option<Footprint> {
        let Focus::Cell { x, y } = self.focus else { return None };
        let (size, before) = (self.pen_size, (self.pen_size - 1) / 2);
        let span = |at: usize, len: usize| at.saturating_sub(before)..(at + size - before).min(len);
        Some(Footprint { xs: span(x, self.canvas.width()), ys: span(y, self.canvas.height()) })
    }

    /// The view as it will be drawn in a terminal `width` by `height` — the window moved as far as
    /// it must to keep the cursor in it, and no further, then held inside the picture.
    ///
    /// Pure, so [`render`] can call it on a borrowed picker and always show the cursor, and
    /// [`run`] can store the answer so the next frame starts from where this one ended.
    pub fn scrolled(&self, width: usize, height: usize) -> View {
        let rows = body_rows(height, self.warns_of_memory());
        let cols = pane_columns(width, self.split);
        let mut view = self.view;
        view.rows = rows.max(1);
        if view.follow {
            // The dial's lines above and below the cursor are part of what must be seen — and the
            // cursor's own line has the last word, on a terminal too short for all of them.
            if let Some((first, _)) = self.dial_lines() {
                view.top = follow(view.top, first + DialLine::ALL.len() - 1, rows);
                view.top = follow(view.top, first, rows);
            }
            view.top = follow(view.top, self.cursor_line(), rows);
            if let Focus::Cell { x, .. } = self.focus {
                view.left = follow(view.left, x, cols);
            }
        }
        view.top = view.top.min(self.body_len().saturating_sub(rows));
        view.left = view.left.min(self.canvas.width().saturating_sub(cols));
        view
    }

    /// Remember the view a frame was drawn with, so the picture holds still between frames.
    pub fn set_view(&mut self, view: View) {
        self.view = view;
    }

    /// The view the last frame was drawn with — see [`Picker::set_view`].
    pub fn view(&self) -> View {
        self.view
    }
}

/// Body lines on screen for a terminal `height` rows tall: all of it but the pinned footer.
fn body_rows(height: usize, memory_warning: bool) -> usize {
    height.saturating_sub(footer(height, memory_warning).len())
}

/// One line of the footer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FooterLine {
    /// What state the work is in — see [`status`].
    Status,
    /// Keys for the file as a whole: leaving, saving, taking back.
    File,
    /// Keys for colour: picking one, adding one, painting and erasing with it.
    Colour,
    /// Keys for what is on screen: the split, the window over a big picture, a redraw.
    View,
    /// Where the memory goes, in red, once there is a great deal of it — see
    /// [`MEMORY_WARNING_ABOVE`]. Only while there is.
    Memory,
}

/// Rows the drawing keeps before the footer spends any on its second and third hint lines.
/// terminal_choice's threshold for giving up its pinned footer, borrowed for the same reason:
/// below three rows there is too little picture to work in, and help about keys is worth less
/// than room to use them.
const MIN_BODY_ROWS: usize = 3;

/// The footer of a terminal `height` rows tall, top to bottom — PINNED at every height, and shed
/// from what is least needed when there is no room. The file line is the last to go, because it
/// says how to leave, and that is never the thing to scroll away. The status goes before it,
/// because a pen that is down without the person knowing paints where they do not mean to.
///
/// The memory warning, while there is one, comes next: it is news rather than help, so it takes a
/// row before the other two hint lines do, and it sits at the very bottom, under all of them.
/// Those two come and go with the room the drawing keeps: see [`MIN_BODY_ROWS`].
fn footer(height: usize, memory_warning: bool) -> Vec<FooterLine> {
    use FooterLine::{Colour, File, Memory, Status, View};
    let mut lines = match height {
        0 => Vec::new(),
        1 => vec![File],
        _ => vec![Status, File],
    };
    if memory_warning && height > lines.len() {
        lines.push(Memory);
    }
    for hint in [Colour, View] {
        if height >= lines.len() + 1 + MIN_BODY_ROWS {
            let above_the_warning = lines.len() - usize::from(lines.last() == Some(&Memory));
            lines.insert(above_the_warning, hint);
        }
    }
    lines
}

/// Art columns on screen for a terminal `width` columns wide — in EACH pane, when the canvas and
/// its preview are side by side: all of it but the cursor gutter and the border around the art.
fn pane_columns(width: usize, split: Option<Split>) -> usize {
    let room = width.saturating_sub(GUTTER);
    match split {
        Some(Split::SideBySide) => room.saturating_sub(3) / 2,
        _ => room.saturating_sub(2),
    }
}

/// Where a window of `size` starting at `start` must start to contain `at` — unchanged if it
/// already does, otherwise moved just far enough.
fn follow(start: usize, at: usize, size: usize) -> usize {
    match size {
        0 => at,
        _ if at < start => at,
        _ if at >= start + size => at + 1 - size,
        _ => start,
    }
}

/// Appended to a document's path to name where an interrupted session's work is kept.
const SALVAGE_SUFFIX: &str = ".arterminal.tmp";

/// Whether moving the cursor changes the cells it passes over.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Pen {
    /// Moving just moves.
    #[default]
    Up,
    /// Every cell the cursor lands on is painted with the brush.
    Painting,
    /// Every cell the cursor lands on is cleared.
    Erasing,
}

/// Which part of the picture is on screen.
///
/// The picture holds still while the cursor moves inside the window and slides only as far as it
/// must when the cursor reaches an edge — the rule terminal_choice's viewport uses, so that moving
/// around a large drawing never makes it jump. Shift with an arrow moves the window without the
/// cursor, which is how a part the cursor never visits — the split view's preview — is seen; the
/// next cursor move brings the window back to follow it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct View {
    /// First line of the body on screen.
    pub top: usize,
    /// First column of the art on screen.
    pub left: usize,
    /// Whether the window is following the cursor, or was moved away from it on purpose.
    pub follow: bool,
    /// How many body lines the window showed last time — what a page is, for Page Up/Down.
    pub rows: usize,
}

impl Default for View {
    fn default() -> Self {
        Self { top: 0, left: 0, follow: true, rows: 1 }
    }
}

/// Where a picker's memory goes — see [`Picker::memory`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Memory {
    /// The canvas: a cell for every column of every row.
    pub image: usize,
    /// Everything undo and redo can reach, and the lists that hold it.
    pub history: usize,
}

impl Memory {
    pub fn total(self) -> usize {
        self.image + self.history
    }
}

/// Past this much memory — the image and its undo history together — a red line under the hints
/// says so, and says which is which.
///
/// A gigabyte: far past what ordinary art needs — a 200 × 80 drawing is 128 KB, and a long
/// session's history a few megabytes — so the line only ever appears for something out of the
/// ordinary, such as a big pen dragged over a very large canvas, which records every cell it
/// touches; and well before a typical machine of 8 to 16 GB starts to strain. Nothing is capped:
/// the line informs, and the person decides.
pub const MEMORY_WARNING_ABOVE: usize = 1_000_000_000;

/// A number of bytes as a person reads one: 480.0 KB, 1.2 GB. Decimal units, as disks and the
/// question before opening a large file count them.
pub fn readable_bytes(bytes: usize) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    if bytes < 1000 {
        return format!("{bytes} bytes");
    }
    let (mut value, mut unit) = (bytes as f64 / 1000.0, 0);
    while value >= 1000.0 && unit + 1 < UNITS.len() {
        value /= 1000.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

/// How a split view lays out its two halves: the canvas, where every cell is a solid block of its
/// colour, and the preview, which is the art as it will really look.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Split {
    /// The canvas above the preview — F6.
    Stacked,
    /// The canvas left of the preview, row beside row — Shift+F6. Half the width each, and no
    /// scrolling needed to see both.
    SideBySide,
}

/// One line of the body, from the top: the palette, `[+]`, a gap, then the art in its border.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BodyLine {
    Swatch(usize),
    /// One of the dial's lines, while a colour is being dialled: where its swatch is, or will be.
    Dial(DialLine),
    Add,
    Gap,
    /// A solid band of the border: above the art, below it, and between the canvas and the
    /// preview when they are stacked.
    Border,
    /// Row `y` of the art — of the canvas, when split, with the preview's row beside it when
    /// they are side by side.
    Art(usize),
    /// Row `y` of the stacked split's preview.
    Preview(usize),
}

/// The dial's lines, from the top.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DialLine {
    /// A `^` over the value the arrows turn.
    Above,
    /// The colour, and its hue, saturation and brightness.
    Values,
    /// A `v` under the value the arrows turn.
    Below,
    /// In blue: how to type the value exactly — see [`dial_tip`].
    Tip,
}

impl DialLine {
    const ALL: [DialLine; 4] = [DialLine::Above, DialLine::Values, DialLine::Below, DialLine::Tip];
}

/// The cells the pen covers.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Footprint {
    xs: std::ops::Range<usize>,
    ys: std::ops::Range<usize>,
}

impl Footprint {
    /// The columns it covers on row `y`, if it reaches that row.
    fn on_row(&self, y: usize) -> Option<std::ops::Range<usize>> {
        self.ys.contains(&y).then(|| self.xs.clone())
    }
}

/// What is keeping the pen down.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Grip {
    /// Keys held down — Space, Enter, Backspace — each with the pen it asked for, in the order
    /// they went down. The last is in charge; releasing it hands the pen to the one beneath, and
    /// releasing the last of them lifts it. Never empty.
    Held(Vec<(KeyCode, Pen)>),
    /// Latched by `b` or Delete, and lifted by pressing the same toggle again.
    Toggled,
}

/// A label part-way through being typed.
#[derive(Debug, Clone, PartialEq, Eq)]
struct LabelEdit {
    /// Which swatch row.
    at: usize,
    /// What has been typed so far. Starts as the current label, so Enter keeps it.
    text: String,
    /// What to put back on Esc.
    original: String,
    /// Whether this name is the second half of the edit just recorded — `[+]` adding the swatch,
    /// or F2 moving its colour — and is taken back and put back with it, as one.
    joins: bool,
}

/// Which way an edit is replayed: back, as undo does, or forward, as redo does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Way {
    Back,
    Forward,
}

/// A colour part-way through being chosen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ColourEdit {
    target: Target,
    dial: Dial,
}

/// Whose colour is on the dial.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Target {
    /// A swatch `[+]` adds once the colour is kept.
    New,
    /// The swatch `at` rows down, which F2 is recolouring.
    Existing(usize),
}

/// One thing a session did that it may want to take back.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Edit {
    Paint {
        x: usize,
        y: usize,
        from: Option<Ink>,
        to: Option<Ink>,
    },
    /// Several edits taken back and put back as one: everything painted while the pen was down,
    /// so one drag is one undo — or a swatch's adding or new colour, and the name it was given
    /// straight after; see [`Picker::commit_rename`]. Replayed whole or not at all, see
    /// [`Picker::replay`].
    ///
    /// Its parts hold the names in force when each was made, as a [`Edit::Rename`] does, and
    /// last-in-first-out order keeps them true the same way; [`History::rename`] leaves them be.
    Stroke(Vec<Edit>),
    AddSwatch {
        label: String,
        color: Ink,
    },
    /// A swatch's move. Kept as its own truth and never rewritten by [`History::recolour`]: a
    /// later move of the same swatch is a separate entry, and undoing in order walks them back.
    Recolour {
        label: String,
        from: Ink,
        to: Ink,
    },
    /// Not rewritten by [`History::rename`] either, for the same reason — each holds exactly the
    /// names in force at its moment, and last-in-first-out order keeps that true.
    Rename {
        from: String,
        to: String,
    },
}

impl Edit {
    /// Whether replaying this asks anything of the palette, which is what can refuse — see
    /// [`Picker::replay`]. Not for paint alone, which is most of what a history holds.
    fn moves_the_palette(&self) -> bool {
        match self {
            Edit::Paint { .. } => false,
            Edit::Stroke(parts) => parts.iter().any(Edit::moves_the_palette),
            Edit::AddSwatch { .. } | Edit::Recolour { .. } | Edit::Rename { .. } => true,
        }
    }

    /// Visit every edit that replaying this one `way` runs, one at a time, in the order it runs
    /// them: this one, or a stroke's parts — first to last going forward, last to first going
    /// back — however deeply strokes nest. Stops at the first `Err`.
    fn for_each_part<E>(
        &self,
        way: Way,
        visit: &mut impl FnMut(&Edit) -> Result<(), E>,
    ) -> Result<(), E> {
        match (self, way) {
            (Edit::Stroke(parts), Way::Forward) => {
                parts.iter().try_for_each(|part| part.for_each_part(way, visit))
            }
            (Edit::Stroke(parts), Way::Back) => {
                parts.iter().rev().try_for_each(|part| part.for_each_part(way, visit))
            }
            (single, _) => visit(single),
        }
    }

    /// Replaying this one `way` as far as the palette is concerned — the one share of undoing or
    /// redoing that can be refused — done to `palette`: the picker's own, or a copy, to ask
    /// first. The colour changes it made, for the drawing and the history to follow; see
    /// [`Picker::carry_colours`]. THE ONE PLACE an edit is turned into palette calls, so asking
    /// first and doing can never disagree.
    fn apply_to_palette(
        &self,
        palette: &mut Palette,
        way: Way,
    ) -> Result<Vec<(Ink, Ink)>, PaletteError> {
        // Most palette calls move no colours: they answer with nothing, or with the swatch taken
        // out, which is only the palette's to hold.
        fn nothing<T>(_: T) -> Vec<(Ink, Ink)> {
            Vec::new()
        }
        match (self, way) {
            (Edit::Paint { .. }, _) => Ok(Vec::new()),
            (Edit::Stroke(_), _) => unreachable!("a stroke is replayed a part at a time"),
            (Edit::AddSwatch { label, color }, Way::Forward) => {
                palette.push(label.clone(), *color).map(nothing)
            }
            (Edit::AddSwatch { label, .. }, Way::Back) => palette.remove(label).map(nothing),
            (Edit::Recolour { label, from, to }, way) => {
                let colour = match way {
                    Way::Back => *from,
                    Way::Forward => *to,
                };
                palette.set_color(label, colour).map(|change| change.changes().to_vec())
            }
            (Edit::Rename { from, to }, Way::Forward) => palette.rename(from, to).map(nothing),
            (Edit::Rename { from, to }, Way::Back) => palette.rename(to, from).map(nothing),
        }
    }

    /// Bytes this edit keeps on the heap beyond its own slot in a list: a stroke's list of parts
    /// and whatever those keep, and the labels a swatch's edit carries.
    fn heap_bytes(&self) -> usize {
        match self {
            Edit::Paint { .. } => 0,
            Edit::Stroke(parts) => {
                parts.capacity() * std::mem::size_of::<Edit>()
                    + parts.iter().map(Edit::heap_bytes).sum::<usize>()
            }
            Edit::AddSwatch { label, .. } | Edit::Recolour { label, .. } => label.capacity(),
            Edit::Rename { from, to } => from.capacity() + to.capacity(),
        }
    }
}

/// What has been done, what has been taken back, and where the file last agreed with it.
#[derive(Debug, Clone)]
struct History {
    done: Vec<Edit>,
    undone: Vec<Edit>,
    /// How long `done` was when the pen went down, while it is down. Everything pushed since is
    /// one stroke, folded into a single entry when the pen lifts.
    stroke_start: Option<usize>,
    /// How many edits were done when the file was last written — `None` once that point can no
    /// longer be returned to, because something was undone past it and then something new done.
    /// Without that, undoing to the save point and editing again could leave `done` the same
    /// LENGTH as when saved with different CONTENT, and the picker would call itself clean.
    saved_at: Option<usize>,
    /// What each list keeps on the heap beyond its own slots — every stroke's list of parts,
    /// every label an edit carries — as running totals, since adding them up afresh would walk
    /// millions of edits on every frame. Every change to either list goes through a method here
    /// that keeps these true. See [`History::bytes`].
    done_heap: usize,
    undone_heap: usize,
}

impl Default for History {
    /// Nothing done, and that is the saved point: a document as it was opened or built agrees
    /// with its source by definition. `saved_at: None` here would make every fresh document
    /// report itself dirty — which is exactly what happened before this was spelled out, and a
    /// live run found it as "Esc on a just-opened file warns about unsaved work".
    fn default() -> Self {
        Self {
            done: Vec::new(),
            undone: Vec::new(),
            stroke_start: None,
            saved_at: Some(0),
            done_heap: 0,
            undone_heap: 0,
        }
    }
}

impl History {
    fn push(&mut self, edit: Edit) {
        if self.saved_at.is_some_and(|at| at > self.done.len()) {
            self.saved_at = None;
        }
        self.undone.clear();
        self.undone_heap = 0;
        self.done_heap += edit.heap_bytes();
        self.done.push(edit);
    }

    /// The last edit done, taken off to be undone.
    fn take_done(&mut self) -> Option<Edit> {
        let edit = self.done.pop()?;
        self.done_heap -= edit.heap_bytes();
        Some(edit)
    }

    /// An edit put back on the done list — redone, or an undo that failed — leaving what can be
    /// redone alone, which [`History::push`] would not.
    fn put_done(&mut self, edit: Edit) {
        self.done_heap += edit.heap_bytes();
        self.done.push(edit);
    }

    /// The last edit undone, taken off to be redone.
    fn take_undone(&mut self) -> Option<Edit> {
        let edit = self.undone.pop()?;
        self.undone_heap -= edit.heap_bytes();
        Some(edit)
    }

    /// An edit put on the undone list — undone, or a redo that failed.
    fn put_undone(&mut self, edit: Edit) {
        self.undone_heap += edit.heap_bytes();
        self.undone.push(edit);
    }

    /// What the history keeps in memory: both lists' own slots, as allocated, and everything that
    /// hangs off them — see [`Edit::heap_bytes`].
    fn bytes(&self) -> usize {
        (self.done.capacity() + self.undone.capacity()) * std::mem::size_of::<Edit>()
            + self.done_heap
            + self.undone_heap
    }

    fn is_dirty(&self) -> bool {
        self.saved_at != Some(self.done.len())
    }

    fn mark_saved(&mut self) {
        self.saved_at = Some(self.done.len());
    }

    fn begin_stroke(&mut self) {
        self.stroke_start = Some(self.done.len());
    }

    fn in_stroke(&self) -> bool {
        self.stroke_start.is_some()
    }

    /// Fold everything pushed since [`History::begin_stroke`] into one entry. A stroke of one
    /// paint stays a plain paint; a stroke of none leaves no trace.
    fn end_stroke(&mut self) {
        let Some(start) = self.stroke_start.take() else { return };
        let mut parts = self.done.split_off(start.min(self.done.len()));
        match parts.len() {
            0 => {}
            1 => self.done.push(parts.remove(0)),
            _ => {
                // The parts' own heap is counted already, from when they were entries of their
                // own; only the list that holds them now is new.
                self.done_heap += parts.capacity() * std::mem::size_of::<Edit>();
                self.done.push(Edit::Stroke(parts));
            }
        }
        // A long stroke grew `done` to hold every cell it touched, and folding them into one entry
        // leaves that room empty. Give it back: it is not history, and kept, it would be counted
        // as history and cost as much again as the stroke itself.
        if self.done.capacity() > 2 * self.done.len() + 64 {
            self.done.shrink_to_fit();
        }
    }

    /// Carry a swatch's move into every edit that mentions its old colour.
    fn recolour(&mut self, was: Ink, now: Ink) {
        fn visit(edit: &mut Edit, was: Ink, now: Ink) {
            let swap = |ink: &mut Option<Ink>| {
                if *ink == Some(was) {
                    *ink = Some(now);
                }
            };
            match edit {
                Edit::Paint { from, to, .. } => {
                    swap(from);
                    swap(to);
                }
                Edit::Stroke(parts) => parts.iter_mut().for_each(|part| visit(part, was, now)),
                Edit::AddSwatch { color, .. } => {
                    if *color == was {
                        *color = now;
                    }
                }
                // Each of these is its own truth about a moment — see [`Edit`].
                Edit::Recolour { .. } | Edit::Rename { .. } => {}
            }
        }
        for edit in self.done.iter_mut().chain(self.undone.iter_mut()) {
            visit(edit, was, now);
        }
    }

    /// Carry a swatch's new name into every edit on the lists that held the old one. Not into a
    /// stroke's parts, which keep the names of their moment — see [`Edit::Stroke`].
    fn rename(&mut self, from: &str, to: &str) {
        let lists =
            [(&mut self.done, &mut self.done_heap), (&mut self.undone, &mut self.undone_heap)];
        for (list, heap) in lists {
            for edit in list.iter_mut() {
                if let Edit::AddSwatch { label, .. } | Edit::Recolour { label, .. } = edit {
                    if label == from {
                        *heap -= label.capacity();
                        *label = to.to_string();
                        *heap += label.capacity();
                    }
                }
            }
        }
    }
}

/// How a run ended.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Outcome {
    /// The user closed the picker. Whatever they built is on the [`Picker`].
    Closed,
    /// The user pressed Ctrl+C. Unsaved work, if there was any and anywhere to put it, was
    /// written to `salvaged` — the document's path with `.arterminal.tmp` appended.
    Interrupted { salvaged: Option<PathBuf> },
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
    /// F5: clear the terminal and draw everything afresh. [`apply`] does not do it — it touches
    /// no terminal — so the caller must.
    Refresh,
    /// The picker should close; the user chose to, and was warned if work was unsaved.
    Close,
    /// Ctrl+C: close at once. The caller should offer unsaved work a home first — see
    /// [`Picker::salvage`] — because an interrupt that argued would not be one, and one that
    /// silently lost the work would be worse.
    Interrupt,
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
/// Their width, which the art's horizontal window leaves room for.
const GUTTER: usize = 2;

/// What an uncoloured cell is drawn as on the split view's canvas: black, as asked for, and
/// explicit rather than the terminal's own background, so the canvas reads as a rectangle on a
/// light theme too.
const UNINKED: Ink = Ink::Rgb(Rgb::new(0, 0, 0));

/// The split view's cursor. ASCII, so it is one column wide on every terminal.
const BLOCK_CURSOR: char = '+';

/// The border around the art. A mid grey: unmistakable against the canvas's black and against
/// every colour `[+]` draws, which are always fully saturated (see [`Rgb::random`]), and visible
/// on light and dark terminal themes alike.
///
/// Drawn as BACKGROUND-coloured spaces rather than box-drawing lines, which are East Asian
/// Ambiguous width — two columns wide on a terminal set up for a CJK locale, shearing the picture
/// — the same reason the swatches are spaces. The price, taken knowingly: with styling off (a
/// redirect, `NO_COLOR`) the border still takes its cells but cannot be seen.
const BORDER: Rgb = Rgb::new(110, 110, 110);

/// Marks a line the terminal was too narrow to show whole.
const CLIPPED: char = '…';

/// Shown beside a label being typed. ASCII on purpose: the arrows and shapes that would look
/// nicer here are East Asian Ambiguous width, and a prompt that shears the row on some
/// terminals is worse than one that is plain.
const RENAME_PROMPT: &str = "  type a name, enter keeps it, esc cancels";

/// Between two values on the dial's middle line, as the layout was specified.
const DIAL_SEPARATOR: &str = " ; ";

/// Columns one value takes on the dial's middle line — see [`dial_value`].
const DIAL_VALUE_WIDTH: usize = "H: 359".len();

/// The column the dial's values start at: where a swatch's label does.
const DIAL_VALUES_AT: usize = GUTTER + SWATCH_WIDTH + LABEL_GAP.len();

/// Between the dial's values and the name of the colour after them.
const DIAL_NAME_GAP: &str = "  ";

/// The column the colour's name starts at, after the dial's values.
const DIAL_NAME_AT: usize = DIAL_VALUES_AT
    + Channel::ALL.len() * DIAL_VALUE_WIDTH
    + (Channel::ALL.len() - 1) * DIAL_SEPARATOR.len()
    + DIAL_NAME_GAP.len();

/// The widest the tip under the dial can be — see [`dial_tip`] — with the widest letter and the
/// longest largest value. `a_dial_tip_ends_before_the_strips` holds every real tip to it.
const DIAL_TIP_WIDEST: &str = "Max H if/when typing: 359. Or: #rrggbb";

/// The column the strips beside the dial start at: two past the widest of the dial's lines —
/// its middle one naming the colour by its longest name, `slot 255`, which a hex colour being
/// typed, caret and all, is shorter than; or the tip under it — so that the strips hold still as
/// numbers change, and nothing the dial says ever runs under them.
const STRIPS_AT: usize = {
    let (middle, tip) = (DIAL_NAME_AT + "slot 255".len(), DIAL_VALUES_AT + DIAL_TIP_WIDEST.len());
    (if middle > tip { middle } else { tip }) + 2
};

/// Columns of colour in each cell of a strip: two, so it reads as a bar rather than a rule.
const STRIP_WIDTH: usize = 2;

/// Columns the strips take in all: one of black before them, then each strip with a column for
/// its marker and one of black after.
const STRIPS_WIDTH: usize = 1 + Channel::ALL.len() * (STRIP_WIDTH + 2);

/// Cells in each strip, top to bottom, where there is room. Eleven, so saturation and brightness
/// run 100, 90 … 0 — a cell for each Page Up — and hue 360, 324 … 0.
const STRIP_CELLS: usize = 11;

/// The fewest cells a strip is drawn with: a top, a bottom, and one between. Two would only repeat
/// the ends of the range, which the numbers already say.
const MIN_STRIP_CELLS: usize = 3;

/// Beside each strip, at the cell nearest its value. ASCII, so one column on every terminal.
const STRIP_MARKER: char = '<';

/// Behind the strips: black, as the layout asked, so nothing the lines beneath were showing —
/// the ends of long labels, the art — interferes with judging a colour.
const BEHIND_THE_STRIPS: Rgb = Rgb::new(0, 0, 0);

/// The letter and marker of the strip whose channel the arrows turn.
const STRIP_ACTIVE: Rgb = Rgb::new(255, 255, 255);

/// The picker as lines of text: the palette, the `[+]` row, the canvas, a status row, a hint.
///
/// `width` and `height` are the terminal's, in columns and rows — and `height` is a HARD limit
/// this function never exceeds, because a frame taller than the terminal scrolls, and a scrolled
/// frame breaks the painter's cursor arithmetic permanently rather than cosmetically. Pass
/// [`height_budget`] rather than the raw row count: the two differ by one, because every line of
/// a frame ends with a newline, the last one included.
///
/// Neither limit is met by dropping things silently. A picture larger than the terminal is SCROLLED
/// — the window follows the cursor, see [`View`] — and the status row says which rows and columns
/// are on screen, because a drawing that simply stopped at the edge would read as data loss. Any
/// other line too wide for the terminal — a palette row, the status, the hint — ends in `…`.
pub fn render(picker: &Picker, width: usize, height: usize) -> Vec<String> {
    if height == 0 {
        return Vec::new();
    }
    let view = picker.scrolled(width, height);
    let memory_warning = picker.memory_warning();
    let rows = body_rows(height, memory_warning.is_some());
    let cols = pane_columns(width, picker.split);
    let window = view.left..(view.left + cols).min(picker.canvas.width());
    let footprint = picker.footprint();
    let cursor_line = picker.cursor_line();
    // While F2's dial is turned, the frame shows the drawing as keeping the colour would leave it:
    // every colour the change moves, moved. Drawn so, never stored — see [`seen_through`].
    let outcome = picker.colour_edit.as_ref().map(|edit| picker.dial_outcome(edit));
    let moves = match &outcome {
        Some(Ok(moves)) => moves.as_slice(),
        _ => &[],
    };

    // Only the lines that will be on screen are drawn. A 200-row drawing on a 40-row terminal
    // renders the 38 it shows, not all 200 to throw most away.
    let shown = view.top..(view.top + rows).min(picker.body_len());
    let mut lines: Vec<String> = shown
        .clone()
        .map(|line| match picker.body_line(line) {
            BodyLine::Swatch(at) => {
                let swatch = picker.palette.at(at).expect("inside the palette");
                let is_brush = picker.brush.as_deref() == Some(swatch.label());
                let editing = picker.editing.as_ref().filter(|edit| edit.at == at);
                let ink = previewed(swatch.color(), moves);
                swatch_row(swatch, ink, line == cursor_line, is_brush, editing)
            }
            BodyLine::Dial(part) => {
                let edit = picker.colour_edit.as_ref().expect("the dial is open");
                dial_line(picker, edit, part)
            }
            BodyLine::Add => format!("{}{ADD_BUTTON}", mark(line == cursor_line)),
            BodyLine::Gap => String::new(),
            BodyLine::Border => border_row(picker.split, window.len()),
            BodyLine::Art(y) => {
                let row = seen_through(picker.canvas.row(y).expect("inside the art"), moves);
                let marked = line == cursor_line;
                art_line(picker, &row, y, marked, footprint.as_ref(), window.clone())
            }
            // The preview is not a place the cursor can go — the canvas is the same grid, and
            // that is where the painting happens — so it carries no gutter mark. It can still
            // show WHERE the cursor is, which is what `c` turns on and off.
            BodyLine::Preview(y) => {
                let row = seen_through(picker.canvas.row(y).expect("inside the preview"), moves);
                let cursor = footprint.as_ref().and_then(|f| f.on_row(y));
                let cursor = cursor.filter(|_| picker.art_cursor);
                framed(NO_MARK, &[art_cells(&row, cursor, window.clone(), CursorLook::Marker)])
            }
        })
        .collect();
    if let Some(edit) = &picker.colour_edit {
        let dial_line = cursor_line.checked_sub(shown.start).filter(|at| *at < lines.len());
        if let Some(at) = dial_line {
            lay_strips(&mut lines, at, &edit.dial, width, picker.wide_ambiguous);
        }
    }

    // The footer is pinned at every height, and sheds what matters least first — see [`footer`].
    // Scrolling is offered only when there is somewhere to scroll to.
    let scrollable = picker.body_len() > rows || picker.canvas.width() > cols;
    for line in footer(height, memory_warning.is_some()) {
        lines.push(match line {
            FooterLine::Status => {
                let refusal = outcome.as_ref().and_then(|outcome| outcome.as_ref().err());
                status(picker, refusal, &view, shown.clone(), window.clone())
            }
            FooterLine::File => {
                hint_line("Program:", &file_hints(picker), FILE_INK, width, picker.wide_ambiguous)
            }
            FooterLine::Colour => {
                hint_line("Draw:", &colour_hints(picker), COLOUR_INK, width, picker.wide_ambiguous)
            }
            FooterLine::View => hint_line(
                "Display:",
                &view_hints(picker, scrollable),
                VIEW_INK,
                width,
                picker.wide_ambiguous,
            ),
            FooterLine::Memory => {
                let warning = memory_warning.as_deref().unwrap_or_default();
                stderr_style().fg(console::Color::Red).apply_to(warning).to_string()
            }
        });
    }
    for line in &mut lines {
        clip(line, width, picker.wide_ambiguous);
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
///
/// While the label is being typed, its row shows the text so far, a one-cell text cursor, and a
/// prompt in the swatch's own colour — it is the colour just chosen, and the question is what to
/// call it. Cursor and prompt are each one style applied once; see the module docs for why
/// nothing here wraps a rendered string.
///
/// Drawn in `ink`: the swatch's own colour, or where the dial being turned would move it.
fn swatch_row(
    swatch: &Swatch,
    ink: Ink,
    focused: bool,
    is_brush: bool,
    editing: Option<&LabelEdit>,
) -> String {
    let (shown, ink) = (ink, console_color(ink));
    let block = stderr_style().bg(ink).apply_to(" ".repeat(SWATCH_WIDTH));
    match editing {
        Some(edit) => {
            let caret = stderr_style().reverse().apply_to(' ');
            let prompt = stderr_style().fg(ink).bold().apply_to(RENAME_PROMPT);
            format!("{}{block}{LABEL_GAP}# {}{caret}{prompt}", mark(focused), edit.text)
        }
        None => {
            let tag = if is_brush { BRUSH_TAG } else { "" };
            let slot = slot_tag(shown);
            format!("{}{block}{LABEL_GAP}# {}{slot}{tag}", mark(focused), swatch.label())
        }
    }
}

/// After a slot swatch's label, what it is — so two swatches that look alike but are not the
/// same colour can be told apart. The first sixteen say they follow the theme, because only those
/// look different from one terminal to the next. Nothing for a colour of its own.
fn slot_tag(ink: Ink) -> String {
    match ink {
        Ink::Slot(slot) if ink.follows_theme() => {
            format!("  {}", stderr_style().dim().apply_to(format!("slot {slot} (theme)")))
        }
        Ink::Slot(slot) => format!("  {}", stderr_style().dim().apply_to(format!("slot {slot}"))),
        Ink::Rgb(_) => String::new(),
    }
}

/// How `ink` is drawn: a colour of its own as 24-bit colour, a slot as the terminal's own
/// palette entry — so the picker shows a slot exactly as this terminal shows it.
fn console_color(ink: Ink) -> console::Color {
    match ink {
        Ink::Rgb(Rgb { r, g, b }) => console::Color::TrueColor(r, g, b),
        Ink::Slot(slot) => console::Color::Color256(slot),
    }
}

/// `ink` as the frame shows it while the dial is turned: where the change it previews moves it —
/// see [`Picker::dial_outcome`] — or itself.
fn previewed(ink: Ink, moves: &[(Ink, Ink)]) -> Ink {
    moves.iter().find(|(was, _)| *was == ink).map_or(ink, |&(_, now)| now)
}

/// A row of cells as the frame shows it while the dial is turned: every ink the change it
/// previews would move, moved. The canvas itself is untouched — the change is made, and recorded,
/// only when Enter keeps it; see [`Picker::commit_colour`]. The rest of the time, the row as it
/// is, with nothing copied.
fn seen_through<'a>(row: &'a [Cell], moves: &[(Ink, Ink)]) -> Cow<'a, [Cell]> {
    match moves.is_empty() {
        true => Cow::Borrowed(row),
        false => row
            .iter()
            .map(|cell| Cell { ink: cell.ink.map(|ink| previewed(ink, moves)), ..*cell })
            .collect(),
    }
}

/// One of the dial's lines. The second is the colour and its three values; the lines either
/// side point at the value the arrows turn, as the layout was specified, and under them, in blue,
/// is how to type that value exactly:
///
/// ```text
///     ^
/// H:  55 ; S:  64 ; B:  77
///     v
/// Max H if/when typing: 359. Or: #rrggbb
/// ```
fn dial_line(picker: &Picker, edit: &ColourEdit, part: DialLine) -> String {
    let dial = &edit.dial;
    let pointer = |mark: char| format!("{}{mark}", " ".repeat(pointer_column(dial)));
    match part {
        DialLine::Above => pointer('^'),
        DialLine::Values => dial_values(dial, dialled_ink(picker, edit)),
        DialLine::Below => pointer('v'),
        DialLine::Tip => {
            let tip = stderr_style().fg(DIAL_TIP_INK).apply_to(dial_tip(dial));
            format!("{}{tip}", " ".repeat(DIAL_VALUES_AT))
        }
    }
}

/// The tip under the dial: the most the value the arrows turn takes, for when it is typed rather
/// than turned — worded so as not to suggest typing is the only way — and that a colour can be
/// typed in hex instead. While one is, what it takes.
///
/// Plain ASCII, starting where the values do. The strips beside the dial start past the widest
/// it can be, [`DIAL_TIP_WIDEST`], which `a_dial_tip_ends_before_the_strips` holds true.
fn dial_tip(dial: &Dial) -> String {
    match dial.typed_hex() {
        Some(_) => "#rrggbb, or #rgb; esc gives it up".to_string(),
        None => {
            let channel = dial.channel();
            format!("Max {} if/when typing: {}. Or: #rrggbb", channel.letter(), channel.largest())
        }
    }
}

/// The dial's middle line: the colour, its hue, saturation and brightness as shown — each dim
/// while turning it would change nothing, see [`Dial::in_effect`], and underlined while a number
/// is being typed into it — and the colour as the file names it, or in its place the hex colour
/// being typed, with a caret where its next digit goes. The cursor's line, while the dial is open.
fn dial_values(dial: &Dial, ink: Ink) -> String {
    let block = stderr_style().bg(console_color(ink)).apply_to(" ".repeat(SWATCH_WIDTH));
    let values = Channel::ALL
        .map(|channel| {
            let value = dial_value(channel, dial.shown(channel));
            let typing = dial.is_typing_number() && channel == dial.channel();
            match (dial.in_effect(channel), typing) {
                (true, false) => value,
                (in_effect, typing) => {
                    let mut style = stderr_style();
                    if !in_effect {
                        style = style.dim();
                    }
                    if typing {
                        style = style.underlined();
                    }
                    style.apply_to(value).to_string()
                }
            }
        })
        .join(DIAL_SEPARATOR);
    let named = match dial.typed_hex() {
        Some(digits) => format!("#{digits}{}", stderr_style().reverse().apply_to(' ')),
        None => stderr_style().dim().apply_to(ink).to_string(),
    };
    format!("{CURSOR_MARK}{block}{LABEL_GAP}{values}{DIAL_NAME_GAP}{named}")
}

/// One value on the dial's middle line: `H: 355`, `S:  64`. Right-aligned in room for three
/// digits, so no value moves another as it grows. [`DIAL_VALUE_WIDTH`] columns.
fn dial_value(channel: Channel, value: u16) -> String {
    format!("{}: {value:>3}", channel.letter())
}

/// The colour the dial's line shows: F2's swatch's own while the dial has not changed it — so a
/// slot is drawn as the slot until it stops being one — and otherwise the colour dialled.
fn dialled_ink(picker: &Picker, edit: &ColourEdit) -> Ink {
    let own = match edit.target {
        Target::Existing(at) if !edit.dial.is_changed() => picker.palette.at(at).map(Swatch::color),
        _ => None,
    };
    own.unwrap_or(Ink::Rgb(edit.dial.rgb()))
}

/// The column the `^` and `v` stand in: over the middle of the three digits of the channel the
/// arrows turn — or, while a hex colour is typed, over where its next digit goes.
fn pointer_column(dial: &Dial) -> usize {
    if let Some(digits) = dial.typed_hex() {
        return DIAL_NAME_AT + "#".len() + digits.len();
    }
    let at = Channel::ALL.iter().position(|c| *c == dial.channel()).expect("one of the three");
    DIAL_VALUES_AT + at * (DIAL_VALUE_WIDTH + DIAL_SEPARATOR.len()) + DIAL_VALUE_WIDTH - 2
}

/// Lay the strips over the body `lines` around the dial's middle line, `at` among them: centred on
/// it, and moved no further than they must to be wholly on screen. Shorter on a short terminal,
/// down to [`MIN_STRIP_CELLS`]; left off below that, on a terminal too narrow for all of them —
/// half a strip reads as a different colour range — and with styling off, where a strip of colour
/// would show nothing at all and only cut into the lines beneath.
fn lay_strips(lines: &mut [String], at: usize, dial: &Dial, width: usize, wide: bool) {
    let cells = STRIP_CELLS.min(lines.len().saturating_sub(1));
    let fits = width >= STRIPS_AT + STRIPS_WIDTH && cells >= MIN_STRIP_CELLS;
    if !fits || !console::colors_enabled_stderr() {
        return;
    }
    let strips = strips(dial, cells);
    let top = at.saturating_sub(strips.len() / 2).min(lines.len() - strips.len());
    for (line, strip) in lines[top..].iter_mut().zip(&strips) {
        *line = overlay(line, STRIPS_AT, strip, wide);
    }
}

/// The strips beside the dial, a line each from the top: their letters, then `cells` lines of
/// colour — each strip's highest value at the top, see [`Dial::strip`] — with a marker beside the
/// cell nearest each value. The arrows' channel stands out, its letter and marker white and bold,
/// the other two grey. All of it on black, as the layout asked, so that nothing the lines beneath
/// were showing can interfere; every line [`STRIPS_WIDTH`] columns exactly.
fn strips(dial: &Dial, cells: usize) -> Vec<String> {
    let on_black = |text: &str, active: bool| {
        let style = stderr_style().bg(console_color(Ink::Rgb(BEHIND_THE_STRIPS)));
        let style = match active {
            true => style.fg(console_color(Ink::Rgb(STRIP_ACTIVE))).bold(),
            false => style.fg(console_color(Ink::Rgb(TITLE_INK))),
        };
        style.apply_to(text).to_string()
    };
    let spare = " ".repeat(STRIP_WIDTH + 1);
    let mut lines = vec![on_black(" ", false)];
    for channel in Channel::ALL {
        let letter = format!("{}{spare}", channel.letter());
        lines[0].push_str(&on_black(&letter, channel == dial.channel()));
    }
    let columns = Channel::ALL.map(|c| (c, dial.strip(c, cells), dial.marker(c, cells)));
    for cell in 0..cells {
        let mut line = on_black(" ", false);
        for (channel, strip, marker) in &columns {
            let colour = stderr_style().bg(console_color(Ink::Rgb(strip[cell])));
            line.push_str(&colour.apply_to(" ".repeat(STRIP_WIDTH)).to_string());
            let beside = if *marker == cell { STRIP_MARKER } else { ' ' };
            line.push_str(&on_black(&format!("{beside} "), *channel == dial.channel()));
        }
        lines.push(line);
    }
    lines
}

/// Row `y` of the art as the layout shows it, inside its border: the picture itself — or, split,
/// the canvas, with the preview's row beside it when the two are side by side. `row` is its cells
/// as the frame shows them — see [`seen_through`] — and `marked` whether the cursor is on it.
fn art_line(
    picker: &Picker,
    row: &[Cell],
    y: usize,
    marked: bool,
    footprint: Option<&Footprint>,
    window: std::ops::Range<usize>,
) -> String {
    let cursor = footprint.and_then(|f| f.on_row(y));
    let panes = match picker.split {
        None => vec![art_cells(row, cursor, window, CursorLook::Inverted)],
        Some(Split::Stacked) => vec![block_cells(row, cursor, window)],
        Some(Split::SideBySide) => {
            let beside = cursor.clone().filter(|_| picker.art_cursor);
            let preview = art_cells(row, beside, window.clone(), CursorLook::Marker);
            vec![block_cells(row, cursor, window), preview]
        }
    };
    framed(mark(marked), &panes)
}

/// `panes` side by side after the gutter `mark`, with a cell of border before, between and after
/// them. Each piece keeps its own styles; nothing is wrapped around another.
fn framed(mark: &str, panes: &[String]) -> String {
    let edge = border_cell();
    let mut line = format!("{mark}{edge}");
    for pane in panes {
        line.push_str(pane);
        line.push_str(&edge);
    }
    line
}

/// A solid band of the border, capping panes `pane` columns wide.
fn border_row(split: Option<Split>, pane: usize) -> String {
    let wide = match split {
        Some(Split::SideBySide) => 2 * pane + 3,
        _ => pane + 2,
    };
    let Rgb { r, g, b } = BORDER;
    let band = stderr_style().bg(console::Color::TrueColor(r, g, b)).apply_to(" ".repeat(wide));
    format!("{NO_MARK}{band}")
}

/// One cell of the border's sides.
fn border_cell() -> String {
    let Rgb { r, g, b } = BORDER;
    stderr_style().bg(console::Color::TrueColor(r, g, b)).apply_to(' ').to_string()
}

/// Whether column `x` is under the cursor's columns on this row.
fn covers(cursor: &Option<std::ops::Range<usize>>, x: usize) -> bool {
    cursor.as_ref().is_some_and(|xs| xs.contains(&x))
}

/// How the cells under the cursor are drawn, in a row of art.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CursorLook {
    /// Inverted in their own ink — the art view, where the glyph under the cursor is the thing
    /// being painted and must stay readable.
    Inverted,
    /// The canvas's own `+` on a cell filled with the ink — the split view's preview, so the eye
    /// finds the same spot in both halves.
    Marker,
}

/// The cells of a row of art as they will look — glyphs in their colours — with the cells under
/// the cursor drawn as `look` says.
///
/// Drawn in RUNS: every stretch of cells that look alike — the same ink, and all under the cursor
/// or none of it — is ONE styled span. Styled cell by cell, a painted row sent a full colour
/// escape per glyph, some twenty bytes where the glyph itself is one, so a frame grew with how
/// much of the picture was painted rather than with what the eye can tell apart. Measured at
/// 120×50 over the large sample art while dragging a 6×6 pen: 34 KB a frame cell by cell, 7 KB in
/// runs — about what the same picture costs unpainted. Each span is still one style applied once,
/// to glyphs the canvas guarantees are plain text.
fn art_cells(
    row: &[Cell],
    cursor: Option<std::ops::Range<usize>>,
    window: std::ops::Range<usize>,
    look: CursorLook,
) -> String {
    let mut line = String::new();
    let end = window.end.min(row.len());
    let mut x = window.start;
    while x < end {
        let (ink, under) = (row[x].ink, covers(&cursor, x));
        let run = row[x..end]
            .iter()
            .enumerate()
            .take_while(|(at, cell)| cell.ink == ink && covers(&cursor, x + at) == under)
            .count();
        let glyphs = row[x..x + run].iter().map(|cell| cell.glyph);
        match (ink, under, look) {
            // The overwhelmingly common case — unpainted glyphs away from the cursor — costs no
            // style and no allocation beyond the line itself.
            (None, false, _) => line.extend(glyphs),
            (_, true, CursorLook::Marker) => {
                line.push_str(&block_cursor(ink.unwrap_or(UNINKED), run))
            }
            _ => {
                let glyphs: String = glyphs.collect();
                line.push_str(&glyph_style(ink, under).apply_to(glyphs).to_string());
            }
        }
        x += run;
    }
    line
}

/// The cells of a row of the split view's canvas: every cell a solid block of its colour — black
/// where it has none — as if each glyph were a white pixel that took the ink.
///
/// Runs of one colour are drawn as ONE styled span of spaces rather than a span per cell. A
/// 78×200 drawing styled cell by cell is some 300 KB of escapes per frame, re-sent on every
/// keystroke; merged, the cost follows the number of colour CHANGES, which is what the eye sees
/// anyway. Each span is still one style applied once — nothing here wraps a rendered string.
fn block_cells(
    row: &[Cell],
    cursor: Option<std::ops::Range<usize>>,
    window: std::ops::Range<usize>,
) -> String {
    let mut line = String::new();
    let row = &row[..window.end.min(row.len())];
    let mut x = window.start;
    while x < row.len() {
        let (ink, under) = (row[x].ink.unwrap_or(UNINKED), covers(&cursor, x));
        let run = row[x..]
            .iter()
            .enumerate()
            .take_while(|(at, cell)| {
                cell.ink.unwrap_or(UNINKED) == ink && covers(&cursor, x + at) == under
            })
            .count();
        if under {
            line.push_str(&block_cursor(ink, run));
        } else {
            let span = stderr_style().bg(console_color(ink)).apply_to(" ".repeat(run));
            line.push_str(&span.to_string());
        }
        x += run;
    }
    line
}

/// The cursor on `run` solid blocks of `ink`: a marker in whichever of black or white stands out
/// from it.
///
/// Not reverse video, which is how the art view marks its cursor: reversing a coloured SPACE
/// swaps its background into the foreground of a glyph that draws nothing, and on a black cell in
/// a dark terminal the cursor would vanish outright.
fn block_cursor(ink: Ink, run: usize) -> String {
    // A slot's brightness by the RGB it usually is — see [`Ink::approximate`]; a guess for the
    // theme's sixteen, and near enough for choosing black or white.
    let Rgb { r, g, b } = ink.approximate();
    // Perceived brightness by the Rec. 601 luma weights, on the 0–255 scale the channels use.
    let luma = (299 * r as u32 + 587 * g as u32 + 114 * b as u32) / 1000;
    let mark = match luma > 127 {
        true => console::Color::TrueColor(0, 0, 0),
        false => console::Color::TrueColor(255, 255, 255),
    };
    stderr_style()
        .bg(console_color(ink))
        .fg(mark)
        .bold()
        .apply_to(BLOCK_CURSOR.to_string().repeat(run))
        .to_string()
}

/// How glyphs of art look: in their ink, if they have one, and inverted under the cursor.
fn glyph_style(ink: Option<Ink>, under_cursor: bool) -> console::Style {
    let mut style = stderr_style();
    if let Some(ink) = ink {
        style = style.fg(console_color(ink));
    }
    if under_cursor {
        style = style.reverse();
    }
    style
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

/// What state the work is in — ordered by URGENCY, because on a narrow terminal the end of this
/// line is what gets clipped. First any notice, since it is news that matters now ("unsaved changes
/// — press again to discard" must never be the part cut off); then the pen, since a pen down that
/// the person does not know about paints where they do not mean to; then where the window is,
/// when the picture does not fit; whether the file is behind; and last the brush, which the
/// swatch rows show anyway.
///
/// While the dial is on a colour that could not be kept — the `refusal` — that stands where a
/// notice would, bold, before Enter is ever pressed; a notice, being newer, takes its place.
fn status(
    picker: &Picker,
    refusal: Option<&PaletteError>,
    view: &View,
    shown: std::ops::Range<usize>,
    window: std::ops::Range<usize>,
) -> String {
    let pen = match picker.pen() {
        Pen::Up => None,
        Pen::Painting if picker.pen_is_held() => Some("painting while held"),
        Pen::Erasing if picker.pen_is_held() => Some("erasing while held"),
        Pen::Painting => Some("painting — b stops"),
        Pen::Erasing => Some("erasing — del stops"),
    };
    let brush = match picker.brush() {
        Some(label) => format!("brush: {label}"),
        None => "no brush — space on a swatch picks one".to_string(),
    };
    let mut parts: Vec<String> = pen.into_iter().map(str::to_string).collect();
    if picker.pen_size > 1 {
        // An ASCII x: the multiplication sign is East Asian Ambiguous width.
        parts.push(format!("pen {0}x{0}", picker.pen_size));
    }
    parts.extend(where_the_window_is(picker, view, shown, window));
    if picker.is_dirty() {
        parts.push("modified".to_string());
    }
    parts.push(brush);
    let rest = stderr_style().dim().apply_to(parts.join(" · ")).to_string();
    let refused = refusal.map(|why| match why {
        PaletteError::DuplicateColor { held_by, .. } => {
            format!("taken by {held_by:?} — enter cannot keep it")
        }
        why => format!("enter cannot keep this colour: {why}"),
    });
    match picker.notice().map(str::to_string).or(refused) {
        Some(news) => format!("{} · {rest}", stderr_style().bold().apply_to(news)),
        None => rest,
    }
}

/// "rows 5-40 of 200", "cols 1-60 of 78" — only for the dimensions that do not fit, counted in the
/// art's own rows and columns from one, so they match what a person would count.
fn where_the_window_is(
    picker: &Picker,
    view: &View,
    shown: std::ops::Range<usize>,
    window: std::ops::Range<usize>,
) -> Vec<String> {
    let art = picker.canvas.height();
    let art_lines = picker.first_art_line()..picker.first_art_line() + art;
    let (first, last) = (shown.start.max(art_lines.start), shown.end.min(art_lines.end));
    let mut place = Vec::new();
    if !(shown.start <= art_lines.start && shown.end >= art_lines.end) && first < last {
        let (from, to) = (first - art_lines.start + 1, last - art_lines.start);
        place.push(format!("rows {from}-{to} of {art}"));
    }
    if window.len() < picker.canvas.width() {
        place.push(format!("cols {}-{} of {}", view.left + 1, window.end, picker.canvas.width()));
    }
    place
}

/// A key, and what pressing it does: the unit every hint line is made of. The key is written the
/// way a person reads it — `^S`, `shift+F6` — and the action is drawn in its line's colour, so
/// where one ends and the next begins is never a guess.
type Hint = (&'static str, &'static str);

/// The colour of each hint line's actions. The terminal's own named colours rather than fixed
/// RGB, so a light theme and a dark one each draw them in a shade chosen to read on it.
const FILE_INK: console::Color = console::Color::Yellow;
const COLOUR_INK: console::Color = console::Color::Green;
const VIEW_INK: console::Color = console::Color::Cyan;

/// The colour of the tip under the dial: blue, as asked for. The terminal's own blue, as the hint
/// lines' colours are their terminal's own, so each theme draws it in a shade chosen to read on
/// it.
const DIAL_TIP_INK: console::Color = console::Color::Blue;

/// The grey of each hint line's title. Fixed rather than the terminal's own "bright black",
/// which some themes — Solarized's dark one — make the colour of the background itself.
const TITLE_INK: Rgb = Rgb::new(128, 128, 128);

/// Columns every title is padded to, so each line's keys start in the same column.
const TITLE_WIDTH: usize = "Display: ".len();

/// A hint line for a terminal `width` columns wide: its `title` in grey, then every key plain,
/// every action in `ink`, dim dots between. Each piece is one style applied once, side by side —
/// nothing wraps a rendered string, see the module docs.
///
/// THE TITLE GIVES WAY FIRST. When the terminal is too narrow for the title and the line's first
/// hint whole — on the file line, the way out — the title is left off, rather than letting a
/// label crowd out the one key that must never be cut.
fn hint_line(title: &str, hints: &[Hint], ink: console::Color, width: usize, wide: bool) -> String {
    let Some(&(first_key, first_does)) = hints.first() else { return String::new() };
    let dot = stderr_style().dim().apply_to(" · ").to_string();
    let action = stderr_style().fg(ink);
    let body = hints
        .iter()
        .map(|&(key, does)| match key {
            "" => action.apply_to(does).to_string(),
            key => format!("{key} {}", action.apply_to(does)),
        })
        .collect::<Vec<_>>()
        .join(&dot);
    let first = text_width(first_key, wide)
        + usize::from(!first_key.is_empty())
        + text_width(first_does, wide);
    // A line that goes on past its first hint is clipped with a `…`, which takes room too.
    let clipped_after = if hints.len() > 1 { char_width('…', wide) } else { 0 };
    if TITLE_WIDTH + first + clipped_after > width {
        return body;
    }
    let Rgb { r, g, b } = TITLE_INK;
    let label = stderr_style().fg(console::Color::TrueColor(r, g, b)).apply_to(title);
    let pad = " ".repeat(TITLE_WIDTH.saturating_sub(text_width(title, wide)));
    format!("{label}{pad}{body}")
}

/// The first hint line: the file as a whole. Every key on each hint line does something right
/// now, so the lines change with the cursor, and what they say is how this terminal behaves.
///
/// HOW TO LEAVE COMES FIRST. A narrow terminal clips the end of a line, and the one hint that must
/// never be the part cut off is the way out.
fn file_hints(picker: &Picker) -> Vec<Hint> {
    // Named the way this terminal can send it: see the redo arm of [`apply`].
    let redo = if picker.shift_on_ctrl { "shift+^Z" } else { "^Y" };
    match picker.editing.is_some() || picker.colour_edit.is_some() {
        // Esc gives up the name or the colour while one is chosen, and everything else waits —
        // but the interrupt.
        true => vec![("^C", "quit")],
        false => {
            vec![
                ("^X/esc", "close"),
                ("^C", "quit"),
                ("^S", "save"),
                ("^Z", "undo"),
                (redo, "redo"),
            ]
        }
    }
}

/// The second: colour — for the row under the cursor, since that is what the keys act on.
fn colour_hints(picker: &Picker) -> Vec<Hint> {
    if picker.editing.is_some() {
        return vec![("", "type a name"), ("enter", "keep it"), ("esc", "cancel")];
    }
    // The ways out before the long way round: on a narrow terminal the end of the line is cut.
    if let Some(edit) = &picker.colour_edit {
        let keep = match edit.target {
            Target::New => "add",
            Target::Existing(_) => "keep",
        };
        return vec![
            ("←→", "H/S/B"),
            ("↑↓", "turn"),
            ("space/enter", keep),
            ("esc", "cancel"),
            ("0-9", "type"),
            ("#", "hex"),
            ("pgup/pgdn", "by 10"),
        ];
    }
    match picker.focus {
        Focus::Swatch { .. } => {
            vec![("space/enter", "pick brush"), ("F2", "recolour & rename"), ("↑↓", "move")]
        }
        Focus::Add => vec![("space/enter", "add a colour, then name it"), ("↑↓", "move")],
        // Said the way this terminal will actually behave: with releases reported, the pen is
        // held; without them, the same keys tap one cell, and only b / del can drag.
        Focus::Cell { .. } => {
            let (paint, erase) = match picker.hold_keys {
                true => (("space", "hold to paint"), ("backspace", "hold to erase")),
                false => (("space", "paint a cell"), ("backspace", "erase a cell")),
            };
            vec![
                paint,
                erase,
                ("b", "toggle painting"),
                ("del", "toggle erasing"),
                ("i", "pick colour"),
                ("[ ]", "pen size"),
            ]
        }
    }
}

/// The third: what is on screen. Empty while a name is typed or a colour dialled, when none of
/// it acts. Scrolling is offered only when the picture is `scrollable` — bigger than the window
/// one way or the other.
fn view_hints(picker: &Picker, scrollable: bool) -> Vec<Hint> {
    if picker.editing.is_some() || picker.colour_edit.is_some() {
        return Vec::new();
    }
    let mut hints = match picker.split {
        None => vec![("F6", "split"), ("shift+F6", "side by side")],
        Some(Split::Stacked) => vec![("F6", "join"), ("shift+F6", "side by side")],
        Some(Split::SideBySide) => vec![("F6", "stack"), ("shift+F6", "join")],
    };
    if picker.split.is_some() {
        hints.push(("c", if picker.art_cursor { "hide art cursor" } else { "show art cursor" }));
    }
    if scrollable {
        hints.push(("shift+arrows", "scroll"));
    }
    if matches!(picker.focus, Focus::Cell { .. }) {
        hints.push(("pgup/pgdn", "page"));
    }
    hints.push(("F5", "redraw"));
    hints
}

/// Columns `c` takes on a terminal that draws East Asian Ambiguous characters `wide`, or not.
fn char_width(c: char, wide: bool) -> usize {
    use unicode_width::UnicodeWidthChar;
    match wide {
        true => c.width_cjk(),
        false => c.width(),
    }
    .unwrap_or(0)
}

/// Columns a rendered line takes — its escape sequences none — on a terminal that draws East
/// Asian Ambiguous characters `wide`, or not.
fn text_width(text: &str, wide: bool) -> usize {
    segments(text).filter_map(Result::ok).map(|c| char_width(c, wide)).sum()
}

/// A rendered line's pieces, in order: `Ok` for each character that reaches the screen, `Err`
/// for each control sequence — which in a line this module drew is only ever an SGR — whole.
fn segments(line: &str) -> impl Iterator<Item = Result<char, &str>> {
    let mut rest = line;
    std::iter::from_fn(move || {
        let c = rest.chars().next()?;
        if c == '\x1b' && rest[1..].starts_with('[') {
            let end = rest[2..]
                .find(|c: char| ('\x40'..='\x7e').contains(&c))
                .map_or(rest.len(), |at| 2 + at + 1);
            let (escape, after) = rest.split_at(end);
            rest = after;
            return Some(Err(escape));
        }
        rest = &rest[c.len_utf8()..];
        Some(Ok(c))
    })
}

/// Trim `line` to `width` columns in place, as the terminal will measure them — `wide` when it
/// draws East Asian Ambiguous characters two columns wide — marking it when anything was cut.
///
/// Clipping by DISPLAY WIDTH rather than by characters or bytes is the whole point: a wrapped
/// line occupies two physical rows, and then the frame is taller than the painter believes it to
/// be and the cursor arithmetic drifts. Measuring the terminal's way is the same point once more:
/// on a terminal set wide, the `·` between hints is two columns, and a line measured as fitting
/// by the narrow width wraps all the same.
///
/// Cutting a styled line could leave its colour switched on, and the very next thing
/// [`crate::paint::frame`] writes is `\x1b[K` — which on most terminals erases using the CURRENT
/// background. A swatch row clipped mid-colour would then paint itself across the rest of the
/// screen. So a cut that leaves a style on closes it before the `…` — [`head`] does — and the
/// guarantee is pinned by `no_rendered_line_leaves_a_style_switched_on`.
///
/// Our own rather than `console::truncate_str`, which it replaced: that measures only the narrow
/// way. A line that fits is left exactly as it was, with nothing allocated.
fn clip(line: &mut String, width: usize, wide: bool) {
    // No room even for the `…` that says something was cut.
    if width == 0 {
        line.clear();
        return;
    }
    if text_width(line, wide) <= width {
        return;
    }
    let mark = char_width(CLIPPED, wide);
    let (mut cut, _) = head(line, width.saturating_sub(mark), wide);
    if mark <= width {
        cut.push(CLIPPED);
    }
    *line = cut;
}

/// `patch` laid over `line` from column `at`, as if drawn on top of it: the line up to `at` —
/// padded with spaces when it is shorter — then the patch, then the line again from where the
/// patch ends, looking as it did there. Measured as the terminal measures, `wide` or not.
///
/// A glyph that either edge cuts through gives way to spaces, so the patch starts at `at` exactly
/// and what follows it is not pushed along — a wide glyph cut in two would otherwise shift the
/// rest of its line a column, on that line alone. `patch` must close every style it opens, as
/// everything [`render`] draws does.
fn overlay(line: &str, at: usize, patch: &str, wide: bool) -> String {
    let (mut laid, used) = head(line, at, wide);
    laid.push_str(&" ".repeat(at - used));
    laid.push_str(patch);
    laid.push_str(&tail(line, at + text_width(patch, wide), wide));
    laid
}

/// The start of `line`, as many whole glyphs as fit in `room` columns, with any style it leaves on
/// closed — and how many columns that is. The one place a rendered line is cut short: see
/// [`clip`] for why the style must be closed.
fn head(line: &str, room: usize, wide: bool) -> (String, usize) {
    let (mut cut, mut used, mut styled) = (String::new(), 0, false);
    for segment in segments(line) {
        match segment {
            Err(escape) => {
                styled = !matches!(escape, "\x1b[0m" | "\x1b[m");
                cut.push_str(escape);
            }
            Ok(c) => {
                let takes = char_width(c, wide);
                if used + takes > room {
                    break;
                }
                cut.push(c);
                used += takes;
            }
        }
    }
    if styled {
        cut.push_str("\x1b[0m");
    }
    (cut, used)
}

/// The rest of `line` from column `from` on, opening in whatever style is in force there, so it
/// looks as it did in place. A glyph `from` cuts through shows as spaces for its part after the
/// cut. Nothing, when the line ends first.
fn tail(line: &str, from: usize, wide: bool) -> String {
    // Every style switched on since the last reset, replayed in order, is the style in force.
    let (mut rest, mut used, mut in_force, mut started) = (String::new(), 0, String::new(), false);
    for segment in segments(line) {
        match segment {
            Err(escape) if started => rest.push_str(escape),
            Err("\x1b[0m" | "\x1b[m") => in_force.clear(),
            Err(escape) => in_force.push_str(escape),
            Ok(c) if started => rest.push(c),
            Ok(c) => {
                let begins = used;
                used += char_width(c, wide);
                if used <= from {
                    continue;
                }
                rest.push_str(&in_force);
                started = true;
                match begins >= from {
                    true => rest.push(c),
                    false => rest.push_str(&" ".repeat(used - from)),
                }
            }
        }
    }
    rest
}

/// The whole keyboard contract, in one testable place:
/// - `↑`/`↓`/`←`/`→` move the cursor, and keep moving while held; Tab and Shift-Tab are `↓` and
///   `↑`. With a pen down, every cell the cursor lands on is painted or erased; leaving the
///   canvas lifts it.
/// - `Space` or `Enter` does the one thing the row under the cursor is for: on a swatch it picks
///   it as the brush, on `[+]` it opens the colour dial for a new swatch, on a cell it PAINTS —
///   for as long as it is held, or one cell as a tap. `Backspace` on a cell erases the same way.
///   With several held at once, the last pressed is in charge; see [`Picker::hold_pen`].
/// - `b` toggles the painting pen and `Delete` the erasing one: down on one press, up on the
///   next, whatever the terminal can report. See "Holding" below.
/// - `i` on a cell makes its colour the brush. `]` and `[` grow and shrink the pen, a square
///   from 1×1 to the canvas's longer side; see [`Picker::grow_pen`].
/// - `Shift` with an arrow moves the window over a picture too big for the terminal, without the
///   cursor; `Page Up`/`Page Down` move the cursor a window's height. See [`View`].
/// - `F2` on a swatch opens the colour dial on its colour. `F5` asks the caller to clear the
///   screen and draw everything again. `F6` splits the art into a canvas over a preview, and
///   `Shift+F6` into the two side by side; each puts it back together from its own layout. In a
///   split, `c` shows or hides the cursor on the preview.
/// - While a colour is on the dial, `←`/`→` — or `Tab`/`Shift+Tab` — choose hue, saturation or
///   brightness, `↑`/`↓` turn it a step and `Page Up`/`Page Down` ten, all of them repeating while
///   held. Digits type a value straight in and `#` a whole colour in hex; see
///   [`Dial::type_char`]. `Enter` or `Space` keeps the colour — adding `[+]`'s swatch, or
///   recolouring F2's if the colour changed — and goes on to its name; `Esc` gives up a hex colour
///   half typed, and otherwise the dial. Nothing else acts until one of those.
/// - While a name is being typed, keys type, `Backspace` deletes, `Enter` keeps it, `Esc` gives
///   it up. Nothing else acts until one of those. A name given straight after the dial is one
///   undo with the colour; see [`Picker::commit_rename`].
/// - `Ctrl+S` asks for a save — asks, because this function touches no file.
/// - `Ctrl+Z` undoes, repeatedly if held. `Ctrl+Shift+Z` redoes where the terminal can tell it
///   from `Ctrl+Z` — one speaking the kitty keyboard protocol reports the Shift — and `Ctrl+Y`
///   redoes everywhere, because a classic terminal sends the identical byte for `Ctrl+Shift+Z` as
///   for `Ctrl+Z`, and there it can only undo.
/// - `Esc` or `Ctrl+X` closes — unless there is unsaved work, in which case the first press only
///   warns and the second discards. `Ctrl+C` interrupts at once regardless, giving up any name
///   half-typed and asking the caller to salvage what is unsaved.
///
/// # Holding
///
/// Two kinds of pen key, kept apart because they mean different things. HOLD keys — Space, Enter,
/// Backspace — work while held and never latch: where releases are reported the pen is down
/// exactly while the key is, and elsewhere a press is a tap of one cell. TOGGLE keys — `b`,
/// Delete — latch on one press and unlatch on the next, which needs no releases at all, so they
/// drag the same way on every terminal.
///
/// Whether releases arrive is the picker's to be told ([`Picker::set_hold_keys`]); [`run`] tells
/// it from what the terminal answered — or, per stroke, from whether the input devices can see
/// the key. A terminal can answer the protocol query and still drop releases: tmux passes the
/// CSI-u ENCODING but not event types (its issue #3335; full support is open PRs #5600 and #5615
/// at the time of writing). A second press of a held key with no release between is the evidence,
/// and then the picker stops relying on releases rather than leaving a pen stuck down — see
/// [`Picker::hold_pen`]. The status row always shows whether a pen is down and what holds it, so
/// a pen that disagrees with the key under a finger is visible, never silent.
///
/// # What does not count as a keystroke
///
/// Releases and lone modifier keys never clear a notice or withdraw a close warning. Under the
/// protocol, releasing Esc is an event of its own — and if it counted, the release of the first
/// Esc would disarm the "press again to discard" it had just armed, and the confirmation could
/// never complete.
pub fn apply(picker: &mut Picker, event: impl Into<KeyEvent>) -> Action {
    let event = event.into();
    if event.code == KeyCode::Modifier {
        return Action::Ignored;
    }
    if event.kind == KeyKind::Release {
        return released(picker, event);
    }

    let had_notice = picker.notice.take().is_some();
    let armed = std::mem::take(&mut picker.quit_armed);
    let pressed = event.kind == KeyKind::Press;
    let plain = !event.mods.ctrl && !event.mods.alt;

    let action = if event.is_ctrl('c') {
        picker.cancel_rename();
        picker.cancel_colour();
        picker.lift_pen();
        Action::Interrupt
    } else if picker.editing.is_some() {
        apply_to_rename(picker, event)
    } else if picker.colour_edit.is_some() {
        apply_to_dial(picker, event)
    } else {
        match event.code {
            KeyCode::Escape if pressed => close_requested(picker, armed),
            KeyCode::Char('x') if pressed && event.is_ctrl('x') => close_requested(picker, armed),
            KeyCode::Char('s') if pressed && event.is_ctrl('s') => Action::Save,
            // Redo is Ctrl+Shift+Z wherever it can be told from Ctrl+Z — the kitty keyboard
            // protocol reports the Shift — and Ctrl+Y everywhere, because a classic terminal
            // sends one byte for both chords and there Ctrl+Shift+Z can only ever undo.
            KeyCode::Char('z' | 'Z') if event.mods.ctrl && event.mods.shift && !event.mods.alt => {
                redraw_if(picker.redo())
            }
            KeyCode::Char('z') if event.is_ctrl('z') => redraw_if(picker.undo()),
            KeyCode::Char('y') if event.is_ctrl('y') => redraw_if(picker.redo()),
            KeyCode::Up if event.mods.shift => scroll_view(picker, -1, 0),
            KeyCode::Down if event.mods.shift => scroll_view(picker, 1, 0),
            KeyCode::Left if event.mods.shift => scroll_view(picker, 0, -1),
            KeyCode::Right if event.mods.shift => scroll_view(picker, 0, 1),
            KeyCode::PageUp => page(picker, Dir::Up),
            KeyCode::PageDown => page(picker, Dir::Down),
            KeyCode::Up | KeyCode::BackTab => moved(picker, Dir::Up),
            KeyCode::Down | KeyCode::Tab => moved(picker, Dir::Down),
            KeyCode::Left => moved(picker, Dir::Left),
            KeyCode::Right => moved(picker, Dir::Right),
            KeyCode::F(2) if pressed => match picker.focus {
                Focus::Swatch { at } => redraw_if(picker.begin_recolour(at)),
                _ => Action::Ignored,
            },
            KeyCode::F(5) if pressed => Action::Refresh,
            // Each layout's own key takes the split to it, or — pressed in it — back out of it.
            KeyCode::F(6) if pressed => {
                let layout = if event.mods.shift { Split::SideBySide } else { Split::Stacked };
                picker.split = if picker.split == Some(layout) { None } else { Some(layout) };
                Action::Redraw
            }
            KeyCode::Char('i' | 'I') if pressed && plain => redraw_if(picker.pick_colour()),
            KeyCode::Char(']') if pressed && plain => redraw_if(picker.grow_pen()),
            KeyCode::Char('[') if pressed && plain => redraw_if(picker.shrink_pen()),
            KeyCode::Char('c' | 'C') if pressed && plain && picker.split.is_some() => {
                picker.art_cursor = !picker.art_cursor;
                Action::Redraw
            }
            KeyCode::Enter | KeyCode::Char(' ') if pressed && plain => match picker.focus {
                Focus::Swatch { .. } => redraw_if(picker.select_brush()),
                Focus::Add => {
                    picker.begin_new_colour();
                    Action::Redraw
                }
                Focus::Cell { .. } => redraw_if(picker.hold_pen(Pen::Painting, event.code)),
            },
            KeyCode::Backspace if pressed && plain => {
                redraw_if(picker.hold_pen(Pen::Erasing, KeyCode::Backspace))
            }
            KeyCode::Delete if pressed && plain => redraw_if(picker.toggle_pen(Pen::Erasing)),
            KeyCode::Char('b' | 'B') if pressed && plain => {
                redraw_if(picker.toggle_pen(Pen::Painting))
            }
            // Everything else — including every held repeat of a key that acts once — does
            // nothing, so a stuck key cannot machine-gun swatches or saves.
            _ => Action::Ignored,
        }
    };

    // A key that changed nothing still has to repaint if it took a notice or a warning off the
    // status row — the screen would otherwise keep showing what the picker no longer holds — or
    // if it PUT one there: a refusal that explains itself only to a frame that is never drawn is
    // cleared, unseen, by the very next key.
    match action {
        Action::Ignored if had_notice || armed || picker.notice.is_some() => Action::Redraw,
        other => other,
    }
}

/// A key let go. The only thing a release ever does is lift the pen that key holds.
fn released(picker: &mut Picker, event: KeyEvent) -> Action {
    redraw_if(picker.release_key(event.code))
}

/// Esc or Ctrl+X: close, or warn first if that would lose work.
fn close_requested(picker: &mut Picker, armed: bool) -> Action {
    match picker.is_dirty() && !armed {
        true => {
            picker.quit_armed = true;
            picker.notice =
                Some("unsaved changes — press again to discard, or ^S to save".to_string());
            Action::Redraw
        }
        false => Action::Close,
    }
}

/// Keys while a label is being typed. Everything that is not typing, keeping or giving up is
/// swallowed: a stray arrow must not paint, and a stray `Ctrl+S` must not save a half-name.
/// Keeping and giving up act on a press only, so a held Enter commits once rather than
/// committing and then going on to act on the row underneath.
fn apply_to_rename(picker: &mut Picker, event: KeyEvent) -> Action {
    let pressed = event.kind == KeyKind::Press;
    match event.code {
        KeyCode::Enter if pressed => match picker.commit_rename() {
            Ok(()) => Action::Redraw,
            Err(why) => {
                picker.notice = Some(format!("not renamed: {why}"));
                Action::Redraw
            }
        },
        KeyCode::Escape if pressed => redraw_if(picker.cancel_rename()),
        KeyCode::Backspace => {
            let edit = picker.editing.as_mut().expect("editing");
            redraw_if(edit.text.pop().is_some())
        }
        _ => match event.text {
            Some(c) if !c.is_control() && !event.mods.ctrl && !event.mods.alt => {
                picker.editing.as_mut().expect("editing").text.push(c);
                Action::Redraw
            }
            _ => Action::Ignored,
        },
    }
}

/// Keys while a colour is on the dial. Left and Right — or Tab and Shift+Tab, as in a form —
/// choose hue, saturation or brightness; Up and Down turn it a step, Page Up and Page Down ten,
/// all on every repeat too, since holding is how a dial is turned a long way. Digits type a value
/// straight in and `#` a whole colour in hex, with no mode to switch into, since neither means
/// anything else here — see [`Dial::type_char`] — and Backspace takes back what was typed.
///
/// Enter or Space keeps the colour and goes on to its name, as the two do the same thing
/// everywhere else. Esc gives up a hex colour half typed, and otherwise the dial. Those act on a
/// press only, as while a name is typed. Everything else is swallowed: a stray key must not
/// paint, undo or save while a colour is half chosen.
fn apply_to_dial(picker: &mut Picker, event: KeyEvent) -> Action {
    let pressed = event.kind == KeyKind::Press;
    let typed = event.text.filter(|_| !event.mods.ctrl && !event.mods.alt);
    let dial = &mut picker.colour_edit.as_mut().expect("dialling").dial;
    // What is being typed comes first.
    if typed.is_some_and(|c| dial.type_char(c)) {
        return Action::Redraw;
    }
    if event.code == KeyCode::Backspace {
        return redraw_if(dial.erase());
    }
    if event.code == KeyCode::Escape && pressed && dial.typed_hex().is_some() {
        dial.drop_hex();
        return Action::Redraw;
    }
    // Any other key ends what is being typed before it acts — except that a hex colour which is
    // no colour yet holds every key until it is finished or given up, and says so.
    let ended = match dial.end_typing() {
        Ok(ended) => ended,
        Err(unfinished) => {
            picker.notice = Some(unfinished.to_string());
            return Action::Redraw;
        }
    };
    let action = match event.code {
        KeyCode::Left | KeyCode::BackTab => redraw_if(dial.select(-1)),
        KeyCode::Right | KeyCode::Tab => redraw_if(dial.select(1)),
        KeyCode::Up => redraw_if(dial.turn(1)),
        KeyCode::Down => redraw_if(dial.turn(-1)),
        KeyCode::PageUp => redraw_if(dial.turn(DIAL_PAGE)),
        KeyCode::PageDown => redraw_if(dial.turn(-DIAL_PAGE)),
        KeyCode::Enter | KeyCode::Char(' ') if pressed => {
            if let Err(why) = picker.commit_colour() {
                picker.notice = Some(format!("not kept: {why}"));
            }
            Action::Redraw
        }
        KeyCode::Escape if pressed => redraw_if(picker.cancel_colour()),
        _ => Action::Ignored,
    };
    // Ending a number is a change to the frame even when the key itself did nothing.
    match action {
        Action::Ignored if ended => Action::Redraw,
        action => action,
    }
}

/// How far Page Up and Page Down turn the dial: ten degrees or ten percent, a tenth of the way
/// along saturation and brightness — the jump between two cells of their strips.
const DIAL_PAGE: i32 = 10;

fn redraw_if(changed: bool) -> Action {
    match changed {
        true => Action::Redraw,
        false => Action::Ignored,
    }
}

/// Move the window, not the cursor — how a part the cursor never visits is brought into view.
/// Held inside the picture when next drawn; see [`Picker::scrolled`].
fn scroll_view(picker: &mut Picker, down: isize, right: isize) -> Action {
    let view = &mut picker.view;
    view.follow = false;
    view.top = view.top.saturating_add_signed(down);
    view.left = view.left.saturating_add_signed(right);
    Action::Redraw
}

/// Move the cursor a page — as many rows as the window last showed — within the art.
///
/// A jump, not a stroke: a pen that is down paints nothing along the way, because painting a
/// whole column in one keystroke is not what anyone paging through a picture means.
fn page(picker: &mut Picker, dir: Dir) -> Action {
    let Focus::Cell { x, y } = picker.focus else { return Action::Ignored };
    let last = picker.canvas.height().saturating_sub(1);
    let to = match dir {
        Dir::Up => y.saturating_sub(picker.view.rows),
        _ => (y + picker.view.rows).min(last),
    };
    if to == y {
        return Action::Ignored;
    }
    picker.focus = Focus::Cell { x, y: to };
    picker.view.follow = true;
    Action::Redraw
}

/// Move the cursor, reporting whether anything actually changed. An edge that refuses the move
/// is `Ignored`, not `Redraw` — redrawing an identical frame is work nobody asked for.
fn moved(picker: &mut Picker, dir: Dir) -> Action {
    match picker.focus.step(dir, &picker.palette, &picker.canvas) {
        Some(next) => {
            picker.focus = next;
            picker.view.follow = true;
            match next {
                Focus::Cell { .. } => {
                    picker.apply_pen();
                }
                // Off the canvas the pen has nothing to draw on, and a lifted pen is what a
                // person expects to find when they come back.
                _ => picker.lift_pen(),
            }
            Action::Redraw
        }
        None => Action::Ignored,
    }
}

/// Wait for the terminal, and decode what it sends into keys. `None` when it has gone away.
///
/// Three waits, and the difference between them is the whole of the timeout design: with nothing
/// held, wait as long as the caller allows — for ever when idle, a poll interval while a device
/// holds a stroke; holding a lone `ESC`, wait briefly, since it may simply be the Escape key;
/// holding anything else unfinished, wait long, since no valid input ends there and cutting it
/// short is exactly how a slow link tears a sequence.
fn next_events(
    fd: std::os::fd::RawFd,
    decoder: &mut crate::keys::Decoder,
    wake_ms: i32,
) -> std::io::Result<Option<Vec<KeyEvent>>> {
    use crate::keys::Decoded;
    let timeout = match (decoder.has_partial(), decoder.waiting_on_lone_escape()) {
        (false, _) => wake_ms,
        (true, true) => input::ESCAPE_TIMEOUT_MS,
        (true, false) => input::SEQUENCE_TIMEOUT_MS,
    };
    let decoded = match input::wait_for_input(fd, timeout)? {
        input::Readiness::HungUp => return Ok(None),
        // Woken by the caller's own timer, with nothing half-read: no keys, but a chance to look
        // at the devices again.
        input::Readiness::TimedOut if !decoder.has_partial() => Vec::new(),
        input::Readiness::TimedOut => decoder.flush(),
        input::Readiness::Ready => match input::read_available(fd)? {
            bytes if bytes.is_empty() => return Ok(None), // end of file: the same as a hangup
            bytes => decoder.feed(&bytes),
        },
    };
    // Terminal replies arriving late — a second attributes answer, say — are not keys.
    Ok(Some(
        decoded
            .into_iter()
            .filter_map(|d| match d {
                Decoded::Key(key) => Some(key),
                _ => None,
            })
            .collect(),
    ))
}

/// Show `picker` on the terminal and let the user drive it, until they close it.
///
/// Draws on STDERR, leaving stdout free for a program to print whatever the session produced —
/// the same split the sibling project uses, so `… > out.txt` composes.
///
/// Returns once the user closes the picker, or the terminal goes away. Whatever they built is on
/// the `picker` they lent us, and if they saved it is on disk too.
pub fn run(picker: &mut Picker) -> std::io::Result<Outcome> {
    run_with_devices(picker, None)
}

/// [`run`], holding keys through `devices` — keyboards opened by a process that had the right to
/// and has since given it up; see [`crate::elevate`]. `None` opens whatever this process can
/// read, which is what [`run`] does.
///
/// A terminal that reports releases itself wins either way, and the devices are closed unused:
/// its answer needs no permission and knows which window has the focus.
pub fn run_with_devices(
    picker: &mut Picker,
    devices: Option<crate::InputDevices>,
) -> std::io::Result<Outcome> {
    // BUFFERED, and the difference is the whole of the flicker: see [`crate::paint`]. Same target
    // and same tty-ness as `Term::stderr()` — only the writes are pooled, until an explicit flush.
    let term = Term::buffered_stderr();
    if !term.is_term() {
        return Err(std::io::Error::other("a picker needs a terminal (stderr is not one)"));
    }

    let mut on_screen = 0;
    let mut last_size = term.size();
    let (fd, _tty_handle) = input::input_fd()?;
    // Raw for the whole run (drops — and restores — when this function returns or unwinds).
    let _raw = input::RawMode::engage(fd)?;
    // Keys become events we decode ourselves — see [`crate::keys`] for why not `console`'s reader.
    // Asked for AFTER raw mode, so the terminal's replies arrive byte by byte and unechoed; and
    // declared after `_raw`, so it drops first and the flags come off while still raw.
    let mut decoder = crate::keys::Decoder::new();
    let mut queue = Vec::new();
    let protocol = input::KeyboardProtocol::engage(fd, &mut decoder, &mut queue)?;
    let granted = protocol.as_ref().map(|(_, granted)| *granted).unwrap_or_default();
    let terminal_releases = granted.releases;
    picker.shift_on_ctrl = granted.shift_on_ctrl;
    let _protocol = protocol.map(|(guard, _)| guard);
    // Measured, not guessed — see [`input::ambiguous_is_wide`] — and the locale only when the
    // terminal gave no answer at all.
    picker.wide_ambiguous = match input::ambiguous_is_wide(fd, &mut decoder, &mut queue)? {
        Some(wide) => wide,
        None => input::locale_says_wide(),
    };
    // Where the terminal will not say when a key comes up, the input devices can — if they are
    // readable. Unused when the terminal already reports releases — never opened, or closed at
    // once if they were handed in: the terminal's answer is focus-correct and needs no permission,
    // so the devices would only add exposure.
    let mut holds = match (terminal_releases, devices) {
        (true, _) => None,
        (false, Some(devices)) => Some(crate::devices::Holds::watching(devices.0)),
        (false, None) => crate::devices::Holds::open(),
    };
    picker.hold_keys = terminal_releases || holds.is_some();
    if !picker.hold_keys {
        picker.notice = Some(
            "this terminal cannot report held keys: space taps one cell, b / del toggle — \
             run with --su, or see --help"
                .to_string(),
        );
    }
    let mut queue: VecDeque<KeyEvent> = queue.into();
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
        let (width, height) = (columns as usize, paint::height_budget(rows as usize));
        // Kept for the next frame, so the picture holds still while the cursor moves inside it.
        picker.set_view(picker.scrolled(width, height));
        let lines = render(picker, width, height);
        paint::paint(&term, &lines, on_screen)?;
        on_screen = lines.len();

        // Counted across the whole drain below, not per key: it is the burst that is being
        // bounded, so the tally has to outlive the individual keystrokes it is folding.
        let mut drained = 0;
        let action = loop {
            // A stroke held by a physical key ends when that key comes up — looked at on every
            // pass, so the release is noticed within one poll even while other keys stream in.
            for release in holds.as_mut().map(|h| h.poll_release()).unwrap_or_default() {
                queue.push_front(release);
            }
            let Some(event) = queue.pop_front() else {
                let wake = match holds.as_ref().is_some_and(|h| h.is_holding()) {
                    true => crate::devices::HOLD_POLL_MS,
                    false => -1,
                };
                match next_events(fd, &mut decoder, wake)? {
                    Some(events) => queue.extend(events),
                    // Hangup: the terminal is gone and nobody is there to answer. Treated as an
                    // interrupt rather than a close, so unsaved work is salvaged, not lost with it.
                    None => break Action::Interrupt,
                }
                continue;
            };
            let event = match holds.as_mut() {
                Some(h) if event.kind != KeyKind::Release => {
                    let would_hold =
                        matches!(picker.focus(), Focus::Cell { .. }) && picker.editing().is_none();
                    let (event, then, warn) = h.route(event, would_hold);
                    if let Some(release) = then {
                        queue.push_front(release);
                    }
                    if warn {
                        picker.notice = Some(
                            "holding is not reaching this keyboard (ssh? another keyboard?) — \
                             space taps; b / del toggle"
                                .to_string(),
                        );
                    }
                    event
                }
                _ => event,
            };
            let action = apply(picker, event);
            if let Some(h) = holds.as_mut() {
                h.sync(&picker.held_keys());
            }
            drained += 1;
            // A key that changed nothing never painted anything anyway; a key that did, with more
            // input already waiting behind it, paints once for the whole burst. This is what stops
            // a held arrow key from queueing a full render each.
            let waiting = !queue.is_empty() || input::input_pending(fd);
            let redraws = matches!(action, Action::Redraw);
            if matches!(action, Action::Ignored) || input::coalesce(redraws, waiting, drained) {
                continue;
            }
            break action;
        };
        match action {
            Action::Close => break Outcome::Closed,
            // Unsaved work is offered a home before the door closes. A failure here is the one
            // error that outranks a clean exit: the person asked to leave and their work is
            // about to be lost, so `run` says so rather than returning as if all were well.
            Action::Interrupt => break Outcome::Interrupted { salvaged: picker.salvage()? },
            // The one thing a key can ask for that needs a file. Done here, where the file is
            // reachable, and reported back through the picker so the next frame can say so.
            Action::Save => {
                picker.notice = Some(match picker.save() {
                    Ok(path) => format!("saved to {}", path.display()),
                    Err(err) => format!("not saved: {err}"),
                });
            }
            // Clear everything, then draw from the top as if for the first time. The one place a
            // full clear is right: a person asked for it because the screen is already wrong —
            // resizes reflow scrollback, and rows end up doubled or swallowed. `on_screen = 0`
            // because nothing of the old frame is left to step back over.
            Action::Refresh => {
                term.write_str("\x1b[H\x1b[2J")?;
                on_screen = 0;
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

    /// A footer line of a frame drawn `height` rows tall, found by WHAT it is — through
    /// [`footer`], the layout's own definition — rather than by a position that moves whenever the
    /// footer gains a line or the body above it changes shape.
    fn footer_line(lines: &[String], height: usize, which: FooterLine) -> String {
        let footer = footer(height, false);
        let at = footer.iter().position(|line| *line == which).expect("shown at this height");
        lines[lines.len() - footer.len() + at].clone()
    }

    /// Every hint line of a roomy frame as one plain text, for asking whether a key is offered
    /// at all. Plain, because a hint is the key then its action in colour, and a colour escape
    /// between the two is layout, not content.
    fn hints(picker: &Picker) -> String {
        let lines = render(picker, 300, 60);
        [FooterLine::File, FooterLine::Colour, FooterLine::View]
            .map(|which| console::strip_ansi_codes(&footer_line(&lines, 60, which)).into_owned())
            .join("\n")
    }

    /// Where body line `which` is, through [`Picker::body_line`] — the layout's own map — rather
    /// than by counting the rows above it, which every change to the layout would move.
    fn body_index(picker: &Picker, which: BodyLine) -> usize {
        (0..picker.body_len())
            .position(|at| picker.body_line(at) == which)
            .unwrap_or_else(|| panic!("{which:?} is not in this body"))
    }

    /// The line of a roomy frame of `picker` that shows body line `which`.
    fn line_showing(picker: &Picker, which: BodyLine) -> String {
        assert!(picker.body_len() <= body_rows(60, false), "a roomy frame shows the whole body");
        render(picker, 200, 60)[body_index(picker, which)].clone()
    }

    /// The escape that starts a cell of the border.
    fn border_bg() -> String {
        let Rgb { r, g, b } = BORDER;
        format!("\x1b[48;2;{r};{g};{b}m")
    }

    /// The status row of a roomy frame.
    fn status_of(picker: &Picker) -> String {
        footer_line(&render(picker, 200, 60), 60, FooterLine::Status)
    }

    /// Press `keys` in order, returning what the last one did.
    fn press(picker: &mut Picker, keys: &[KeyEvent]) -> Action {
        let mut last = Action::Ignored;
        for key in keys {
            last = apply(picker, *key);
        }
        last
    }

    /// Bytes as a terminal would send them, through the real decoder and into [`apply`] — so a
    /// test can say "the user pressed F2 on an xterm" rather than naming a decoded event.
    fn type_bytes(picker: &mut Picker, bytes: &[u8]) -> Action {
        let mut decoder = crate::keys::Decoder::new();
        let mut decoded = decoder.feed(bytes);
        decoded.extend(decoder.flush());
        let mut last = Action::Ignored;
        for d in decoded {
            if let crate::keys::Decoded::Key(key) = d {
                last = apply(picker, key);
            }
        }
        last
    }

    const ENTER: KeyEvent = KeyEvent::press(KeyCode::Enter);
    const SPACE: KeyEvent = KeyEvent::press(KeyCode::Char(' '));
    const ESC: KeyEvent = KeyEvent::press(KeyCode::Escape);
    const UP: KeyEvent = KeyEvent::press(KeyCode::Up);
    const DOWN: KeyEvent = KeyEvent::press(KeyCode::Down);
    const LEFT: KeyEvent = KeyEvent::press(KeyCode::Left);
    const RIGHT: KeyEvent = KeyEvent::press(KeyCode::Right);
    const TAB: KeyEvent = KeyEvent::press(KeyCode::Tab);
    const BACKTAB: KeyEvent = KeyEvent::press(KeyCode::BackTab);
    const BACKSPACE: KeyEvent = KeyEvent::press(KeyCode::Backspace);
    const DELETE: KeyEvent = KeyEvent::press(KeyCode::Delete);
    const F2: KeyEvent = KeyEvent::press(KeyCode::F(2));
    const F5: KeyEvent = KeyEvent::press(KeyCode::F(5));
    const F6: KeyEvent = KeyEvent::press(KeyCode::F(6));
    const SAVE: KeyEvent = KeyEvent::ctrl('s');
    const UNDO: KeyEvent = KeyEvent::ctrl('z');
    const REDO: KeyEvent = KeyEvent::ctrl('y');
    const CLOSE: KeyEvent = KeyEvent::ctrl('x');
    const INTERRUPT: KeyEvent = KeyEvent::ctrl('c');

    fn ch(c: char) -> KeyEvent {
        KeyEvent::press(KeyCode::Char(c))
    }

    /// A picker with one swatch picked as the brush and the cursor on the first cell — the state
    /// most painting tests start from. Releases are NOT reported: a hold key taps.
    fn ready_to_paint(art: &str) -> Picker {
        let mut picker = picker(1, art);
        picker.set_focus(Focus::Swatch { at: 0 });
        press(&mut picker, &[ENTER]);
        picker.set_focus(Focus::Cell { x: 0, y: 0 });
        picker
    }

    /// The same, on a terminal that reports key releases — where a hold key really holds.
    fn ready_to_hold(art: &str) -> Picker {
        let mut picker = ready_to_paint(art);
        picker.set_hold_keys(true);
        picker
    }

    const TOGGLE: KeyEvent = KeyEvent::press(KeyCode::Char('b'));

    /// The colour of a swatch these tests gave a colour of its own.
    fn rgb(ink: Ink) -> Rgb {
        ink.rgb().expect("a colour of its own")
    }

    fn inks(picker: &Picker, y: usize) -> Vec<Option<Ink>> {
        picker.canvas().row(y).expect("row").iter().map(|cell| cell.ink).collect()
    }

    // ---- what appears ---------------------------------------------------------------------

    #[test]
    fn the_layout_is_the_palette_then_the_button_then_a_gap_then_the_art() {
        with_colour();
        let picker = picker(2, "ab\ncd");
        let lines = roomy(&picker, Focus::Add);
        assert_eq!(
            lines.len(),
            2 + 1 + 1 + 1 + 2 + 1 + 4,
            "swatches, button, gap, border, art, border, status, 3 hints"
        );
        assert!(lines[2].contains(ADD_BUTTON), "the button follows the swatches: {:?}", lines[2]);
        assert_eq!(lines[3].trim(), "", "a blank row separates the palette from the art");
        for band in [&lines[4], &lines[7]] {
            assert!(band.contains(&border_bg()), "a solid band caps the art: {band:?}");
            assert_eq!(console::strip_ansi_codes(band).trim(), "", "and says nothing: {band:?}");
        }
        let edge = border_cell();
        assert!(lines[5].contains(&format!("{edge}ab{edge}")), "framed: {:?}", lines[5]);
        assert!(lines[6].contains(&format!("{edge}cd{edge}")), "framed: {:?}", lines[6]);
        assert!(lines[8].contains("brush"), "the status row: {:?}", lines[8]);
        assert!(lines[9].contains("close"), "then the file's keys: {:?}", lines[9]);
        assert!(lines[10].contains("add a colour"), "then colour's: {:?}", lines[10]);
        assert!(lines[11].contains("redraw"), "and the view's last: {:?}", lines[11]);
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
        for y in 0..2 {
            let lines = roomy(&picker, Focus::Cell { x: 1, y });
            assert_eq!(marked(&lines), [body_index(&picker, BodyLine::Art(y))], "row {y}");
        }
    }

    #[test]
    fn the_focused_canvas_cell_is_the_only_one_inverted() {
        with_colour();
        let picker = at(&picker(0, "abc"), Focus::Cell { x: 1, y: 0 });
        let row = &line_showing(&picker, BodyLine::Art(0));
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
        picker.canvas_mut().cell_mut(0, 0).expect("in bounds").ink = Some(ink.into());

        let row = &line_showing(&at(&picker, Focus::Add), BodyLine::Art(0));
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
        let status = status_of(&at(&tagged, Focus::Add));
        assert!(status.contains("brush: colour 2"), "{status:?}");

        let none = status_of(&at(&picker(1, "ab"), Focus::Add));
        assert!(none.contains("no brush"), "{none:?}");
    }

    /// The row being renamed shows what has been typed, a one-cell text cursor, and a prompt in
    /// the swatch's own colour — and nothing of the old label or the brush tag.
    #[test]
    fn a_swatch_being_renamed_shows_the_text_a_caret_and_a_coloured_prompt() {
        with_colour();
        let mut picker = picker(0, "ab");
        press(&mut picker, &[ENTER, ENTER]); // [+], its colour kept: added, and named next
        let Rgb { r, g, b } = rgb(picker.palette().at(0).expect("added").color());
        let row = &render(&picker, 200, 60)[0];
        assert!(row.contains("# colour 1"), "the current name is the starting text: {row:?}");
        assert!(
            row.contains(&stderr_style().reverse().apply_to(' ').to_string()),
            "caret: {row:?}"
        );
        assert!(row.contains(RENAME_PROMPT), "{row:?}");
        assert!(
            row.contains(&format!("\x1b[38;2;{r};{g};{b}m")),
            "the prompt wears the swatch's colour: {row:?}"
        );

        press(&mut picker, &[ch('x')]);
        assert!(render(&picker, 200, 60)[0].contains("# colour 1x"), "typing shows at once");
    }

    /// The status row says whether a pen is down, what it does, and what holds it — the indicator
    /// that makes a pen disagreeing with the finger visible rather than silent.
    #[test]
    fn the_status_shows_the_pen_and_what_holds_it() {
        with_colour();
        let status = status_of;
        let mut picker = ready_to_paint("abc");
        press(&mut picker, &[TOGGLE]);
        assert!(status(&picker).contains("painting — b stops"), "{:?}", status(&picker));
        press(&mut picker, &[DELETE]);
        assert!(status(&picker).contains("erasing — del stops"), "{:?}", status(&picker));
        press(&mut picker, &[DELETE]);
        assert!(!status(&picker).contains("ing"), "{:?}", status(&picker));

        let mut held = ready_to_hold("abc");
        press(&mut held, &[SPACE]);
        assert!(status(&held).contains("painting while held"), "{:?}", status(&held));
    }

    /// The hint names the keys that do something on the row under the cursor, so it stays short
    /// and every key on it is live.
    #[test]
    fn the_hint_fits_the_row_under_the_cursor() {
        with_colour();
        let picker = picker(1, "ab");
        assert!(hints(&at(&picker, Focus::Swatch { at: 0 })).contains("F2 recolour"));
        assert!(hints(&at(&picker, Focus::Add)).contains("add a colour"));
        assert!(hints(&at(&picker, Focus::Cell { x: 0, y: 0 })).contains("b toggle painting"));
        assert!(hints(&at(&picker, Focus::Cell { x: 0, y: 0 })).contains("pgup/pgdn page"));
        assert!(!hints(&at(&picker, Focus::Add)).contains("pgup"), "paging is for the canvas");
        let mut editing = picker.clone();
        editing.begin_rename(0);
        let naming = hints(&editing);
        assert!(naming.contains("type a name") && naming.contains("esc cancel"), "{naming}");
        assert!(!naming.contains("close"), "esc gives up the name, it does not close: {naming}");
        for focus in [Focus::Swatch { at: 0 }, Focus::Add, Focus::Cell { x: 0, y: 0 }] {
            assert!(hints(&at(&picker, focus)).contains("^X/esc close"), "the way out, always");
        }
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
        let mut picker = picker(3, "⣿⡇⣿⣿⣿⠛⠁⣴⣿⡿⠿⠧⠹⠿⠘⣿⣿⣿⡇⢸⡻⣿⣿⣿⣿⣿⣿⣿\n⢹⡇⣿⣿⣿⠄⣞⣯⣷⣾⣿⣿⣧⡹⡆⡀⠉⢹⡌⠐⢿⣿⣿⣿⡞⣿⣿⣿");
        // From zero: a terminal with no columns gets empty lines, not a one-column `…`.
        for width in 0..50 {
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
        picker.begin_rename(0); // the widest row there is: text, caret, prompt
        for width in 0..50 {
            for line in render(&picker, width, 20) {
                assert!(
                    console::measure_text_width(&line) <= width,
                    "editing, width {width}: {line:?}"
                );
            }
        }
    }

    /// A canvas too tall for the terminal is scrolled, not truncated: the window follows the
    /// cursor, the footer stays put, and the status says which rows are showing — a drawing that
    /// simply stopped at the bottom edge would read as the whole drawing.
    #[test]
    fn a_picture_taller_than_the_terminal_scrolls_to_keep_the_cursor_in_view() {
        with_colour();
        let mut picker =
            picker(1, &(0..20).map(|n| format!("row{n:02}")).collect::<Vec<_>>().join("\n"));
        picker.set_focus(Focus::Cell { x: 0, y: 19 });
        let lines = render(&picker, 60, 10);
        assert_eq!(lines.len(), 10);
        // The cursor sits on the `r`, so reverse video splits "row19" — look for the rest of it.
        assert!(lines.iter().any(|l| l.contains("ow19")), "the cursor's row is on screen");
        assert_eq!(marked(&lines).len(), 1, "with its mark");
        assert!(!lines.iter().any(|l| l.contains("row00")), "the top has scrolled away");
        let status = footer_line(&lines, 10, FooterLine::Status);
        assert!(status.contains("rows 15-20 of 20"), "and the status says where: {status:?}");
        assert!(lines[7].contains("close"), "the footer stays pinned under the picture");
    }

    /// The picture holds still while the cursor moves inside the window, and slides one row at a
    /// time when the cursor reaches an edge — never jumping.
    #[test]
    fn the_window_moves_only_as_far_as_the_cursor_makes_it() {
        let mut picker = picker(0, &["abc"; 30].join("\n"));
        picker.set_focus(Focus::Cell { x: 0, y: 0 });
        let mut view = picker.scrolled(40, 12);
        assert_eq!(view.top, 0);
        // Down to the last row the window shows, and not past it.
        for _ in 0..body_rows(12, false) - picker.first_art_line() - 1 {
            press(&mut picker, &[DOWN]);
            picker.set_view(picker.scrolled(40, 12));
        }
        assert_eq!(picker.view().top, 0, "moving inside the window leaves it where it was");
        for _ in 0..10 {
            press(&mut picker, &[DOWN]);
            view = picker.scrolled(40, 12);
            picker.set_view(view);
        }
        assert!(view.top > 0, "the cursor went past the bottom, so the window slid");
        assert_eq!(
            picker.cursor_line(),
            view.top + body_rows(12, false) - 1,
            "just far enough to keep the cursor on the last row shown"
        );
    }

    #[test]
    fn a_picture_wider_than_the_terminal_scrolls_sideways() {
        with_colour();
        let wide: String = (0..80).map(|n| char::from(b'a' + (n % 26) as u8)).collect();
        let mut picker = picker(0, &wide);
        picker.set_focus(Focus::Cell { x: 70, y: 0 });
        let lines = render(&picker, 30, 10);
        let art = &lines[body_index(&picker, BodyLine::Art(0)) - picker.scrolled(30, 10).top];
        assert!(
            art.contains(
                &stderr_style().reverse().apply_to(wide.chars().nth(70).unwrap()).to_string()
            ),
            "the cursor's column is on screen: {art:?}"
        );
        assert!(console::measure_text_width(art) <= 30);
        // 26 art columns in 30 — the gutter and the border's two sides take the rest — and the
        // cursor at 0-based 70 is column 71, the last one shown.
        let status = footer_line(&lines, 10, FooterLine::Status);
        assert!(status.contains("cols 46-71 of 80"), "{status:?}");
    }

    /// Shift+arrows move the window without the cursor; the next cursor move brings it back.
    #[test]
    fn shift_arrows_move_the_window_and_the_next_move_brings_it_back() {
        let mut picker = picker(0, &["abc"; 30].join("\n"));
        picker.set_focus(Focus::Cell { x: 0, y: 0 });
        picker.set_view(picker.scrolled(40, 12));
        let shift_down =
            KeyEvent { mods: crate::keys::Mods { shift: true, ..Default::default() }, ..DOWN };
        for _ in 0..5 {
            assert_eq!(apply(&mut picker, shift_down), Action::Redraw);
            picker.set_view(picker.scrolled(40, 12));
        }
        assert_eq!(picker.view().top, 5, "the window moved");
        assert_eq!(picker.focus(), Focus::Cell { x: 0, y: 0 }, "the cursor did not");
        press(&mut picker, &[RIGHT]);
        assert_eq!(
            picker.scrolled(40, 12).top,
            picker.cursor_line(),
            "a cursor move brings the window back to it, just far enough"
        );
    }

    #[test]
    fn the_window_cannot_be_scrolled_past_the_picture() {
        let mut picker = picker(0, &["abc"; 30].join("\n"));
        let shift_down =
            KeyEvent { mods: crate::keys::Mods { shift: true, ..Default::default() }, ..DOWN };
        for _ in 0..100 {
            apply(&mut picker, shift_down);
            picker.set_view(picker.scrolled(40, 12));
        }
        assert_eq!(
            picker.view().top,
            picker.body_len() - body_rows(12, false),
            "the last line of the body is the last shown"
        );
    }

    /// The split view's preview is somewhere the cursor never goes — scrolling the window is how
    /// it is seen on a terminal too short for both halves.
    #[test]
    fn the_split_preview_is_reachable_by_scrolling_the_window() {
        with_colour();
        let mut picker = picker(0, &["wxyz"; 10].join("\n"));
        picker.split = Some(Split::Stacked);
        picker.set_focus(Focus::Cell { x: 0, y: 0 });
        let shift_down =
            KeyEvent { mods: crate::keys::Mods { shift: true, ..Default::default() }, ..DOWN };
        let has_preview = |p: &Picker| render(p, 40, 12).iter().any(|l| l.contains("wxyz"));
        assert!(!has_preview(&picker), "only the canvas fits at first");
        for _ in 0..12 {
            apply(&mut picker, shift_down);
            picker.set_view(picker.scrolled(40, 12));
        }
        assert!(has_preview(&picker), "scrolled down, the preview's glyphs are on screen");
    }

    #[test]
    fn page_keys_move_a_window_at_a_time_and_paint_nothing_on_the_way() {
        let mut picker = ready_to_paint(&["abc"; 40].join("\n"));
        picker.set_view(picker.scrolled(40, 12));
        press(&mut picker, &[TOGGLE, release(KeyCode::Char('b'))]); // pen down at the top
        press(&mut picker, &[KeyEvent::press(KeyCode::PageDown)]);
        let page = body_rows(12, false);
        assert_eq!(picker.focus(), Focus::Cell { x: 0, y: page }, "a window's height down");
        assert!((1..page).all(|y| inks(&picker, y)[0].is_none()), "a jump, not a stroke");
        press(&mut picker, &[KeyEvent::press(KeyCode::PageUp)]);
        assert_eq!(picker.focus(), Focus::Cell { x: 0, y: 0 });
        assert_eq!(
            apply(&mut picker, KeyEvent::press(KeyCode::PageUp)),
            Action::Ignored,
            "at the top"
        );
    }

    /// However little room there is, how to leave must still be on screen — and what the footer
    /// gives up first, as the terminal shrinks, is help about keys rather than the drawing.
    #[test]
    fn the_way_out_survives_the_shortest_terminal() {
        with_colour();
        let picker = picker(2, "ab\ncd");
        for height in 1..12 {
            let lines = render(&picker, 300, height);
            let file = footer_line(&lines, height, FooterLine::File);
            assert!(file.contains("esc"), "height {height} lost the way out: {file:?}");
            let body = lines.len() - footer(height, false).len();
            if footer(height, false).len() > 2 {
                assert!(body >= MIN_BODY_ROWS, "height {height}: extra hints ate the picture");
            }
        }
        assert_eq!(footer(1, false), [FooterLine::File], "one row: only the way out");
        assert_eq!(
            footer(2, false),
            [FooterLine::Status, FooterLine::File],
            "then what the pen is doing"
        );
        assert_eq!(footer(40, false).len(), 4, "and with room, everything");
    }

    /// …and however NARROW. The end of a hint line is what gets clipped, so the way out leads it.
    #[test]
    fn the_way_out_survives_the_narrowest_terminal() {
        with_colour();
        let picker = ready_to_paint("ab");
        for width in 16..80 {
            let file = footer_line(&render(&picker, width, 12), 12, FooterLine::File);
            assert!(file.contains("esc"), "width {width} lost the way out: {file:?}");
        }
    }

    /// Each hint line leads with its title, in grey, padded so the keys of every line start in
    /// the same column.
    #[test]
    fn every_hint_line_has_a_grey_title_and_the_keys_line_up() {
        with_colour();
        let lines = roomy(&ready_to_paint("ab"), Focus::Cell { x: 0, y: 0 });
        let Rgb { r, g, b } = TITLE_INK;
        let grey = format!("\x1b[38;2;{r};{g};{b}m");
        for (which, title) in [
            (FooterLine::File, "Program:"),
            (FooterLine::Colour, "Draw:"),
            (FooterLine::View, "Display:"),
        ] {
            let line = footer_line(&lines, 60, which);
            assert!(line.starts_with(&format!("{grey}{title}")), "{which:?}: {line:?}");
            let plain = console::strip_ansi_codes(&line).into_owned();
            let keys_start = plain.find(|c: char| c != ' ' && !title.contains(c)).expect("keys");
            assert_eq!(keys_start, TITLE_WIDTH, "{which:?} keys start after the title: {plain:?}");
        }
    }

    /// On a terminal too narrow for a title and the way out both, the title gives way — the way
    /// out is never the part a label crowds off.
    #[test]
    fn a_title_gives_way_before_the_way_out_does() {
        with_colour();
        let picker = ready_to_paint("ab");
        let file = |width| {
            let lines = render(&picker, width, 12);
            console::strip_ansi_codes(&footer_line(&lines, 12, FooterLine::File)).into_owned()
        };
        assert!(file(18).starts_with("^X/esc close"), "narrow: {:?}", file(18));
        assert!(file(80).starts_with("Program: ^X/esc close"), "wide: {:?}", file(80));
        for width in 16..80 {
            assert!(file(width).contains("^X/esc close"), "width {width}: {:?}", file(width));
        }
    }

    /// Scrolling is offered only when there is somewhere to scroll to: a picture that fits the
    /// terminal both ways gets no `shift+arrows` hint, and one that does not, either way, does.
    #[test]
    fn the_scroll_hint_shows_only_when_the_picture_does_not_fit() {
        with_colour();
        let view = |p: &Picker, width, height| {
            let lines = render(p, width, height);
            console::strip_ansi_codes(&footer_line(&lines, height, FooterLine::View)).into_owned()
        };
        let small = ready_to_paint("abc\ndef");
        assert!(!view(&small, 80, 30).contains("shift+arrows"), "{}", view(&small, 80, 30));
        let tall = ready_to_paint(&["abc"; 40].join("\n"));
        assert!(view(&tall, 80, 30).contains("shift+arrows scroll"), "taller than the window");
        let wide = ready_to_paint(&"x".repeat(120));
        assert!(view(&wide, 80, 30).contains("shift+arrows scroll"), "wider than the window");
        assert!(view(&small, 80, 30).contains("pgup/pgdn page"), "paging still jumps");
    }

    /// Each hint line colours its actions in its own colour and leaves the keys plain, so "space
    /// pick brush" reads as a key and what it does rather than as three words.
    #[test]
    fn hint_lines_colour_their_actions_and_leave_their_keys_plain() {
        with_colour();
        let lines = roomy(&ready_to_paint("ab"), Focus::Cell { x: 0, y: 0 });
        for (which, ink, key, action) in [
            (FooterLine::File, FILE_INK, "^S", "save"),
            (FooterLine::Colour, COLOUR_INK, "b", "toggle painting"),
            (FooterLine::View, VIEW_INK, "F5", "redraw"),
        ] {
            let line = footer_line(&lines, 60, which);
            let coloured = stderr_style().fg(ink).apply_to(action).to_string();
            assert!(line.contains(&format!("{key} {coloured}")), "{which:?}: {line:?}");
        }
        let colours: std::collections::HashSet<_> = [FILE_INK, COLOUR_INK, VIEW_INK]
            .map(|ink| stderr_style().fg(ink).apply_to("x").to_string())
            .into_iter()
            .collect();
        assert_eq!(colours.len(), 3, "a different colour per line");
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
    /// than a hand-built imitation of them. The row being renamed is included because it is the
    /// most heavily styled line the picker draws.
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
        picker.begin_rename(1);
        for width in 1..60 {
            for line in render(&picker, width, 20) {
                assert!(!leaves_a_style_open(&line), "editing, width {width}: {line:?}");
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
    /// method used — `fg`, `bg`, `reverse`, `dim`, `bold` — to defend a handful of call sites
    /// against a route that requires storing a `console::Style` in a struct, which nothing in this
    /// design has reason to do. The guard is the right weight for the risk.
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
        clip(&mut line, 5, false);
        assert!(line.contains(CLIPPED), "{line:?}");
        assert_eq!(console::measure_text_width(&line), 5);
    }

    #[test]
    fn a_line_that_fits_is_left_exactly_as_it_was() {
        let original = "\x1b[48;2;1;2;3m   \x1b[0m short";
        let mut line = String::from(original);
        clip(&mut line, 80, false);
        assert_eq!(line, original, "nothing was cut, so nothing changed");
    }

    /// On a terminal that draws East Asian Ambiguous characters wide, a line is measured and cut
    /// the way it will be drawn there: two columns for `·`, `—` and `…` alike.
    #[test]
    fn a_wide_terminal_is_measured_and_clipped_its_own_way() {
        assert_eq!((text_width("a·b", false), text_width("a·b", true)), (3, 4));
        assert_eq!(text_width("\x1b[2m·\x1b[0m", true), 2, "escapes take no room");
        let mut line = format!("{}a · b · c · d", stderr_style().dim().apply_to("x"));
        clip(&mut line, 7, true);
        assert!(text_width(&line, true) <= 7, "{line:?}");
        assert!(line.ends_with(CLIPPED), "{line:?}");
        assert!(!leaves_a_style_open(&line), "{line:?}");
        // A two-column character that would not fit is left out whole, so a cut can fall a
        // column short of the width rather than run a column over it.
        let mut narrow = String::from("a·b·c·d");
        clip(&mut narrow, 7, true);
        assert_eq!(narrow, "a·b…", "its `…` two columns, and the `·` after `b` left out whole");
        assert_eq!(text_width(&narrow, true), 6);
    }

    /// Every frame fits a terminal that draws ambiguous characters wide, however narrow — the
    /// same sweep as for the narrow measure, measured the wide way.
    #[test]
    fn no_rendered_line_is_wider_than_a_wide_terminal_draws_it() {
        with_colour();
        let mut picker = picker(3, "a\u{2588}b\u{2591}c\nd\u{b7}e\u{2014}f");
        picker.set_wide_ambiguous(true);
        picker.notice = Some("a notice with a dash — and dots ·…".into());
        for width in 0..60 {
            for focus in [Focus::Swatch { at: 0 }, Focus::Add, Focus::Cell { x: 1, y: 0 }] {
                for line in render(&at(&picker, focus), width, 30) {
                    let measured = text_width(&line, true);
                    assert!(measured <= width, "{measured} columns at {width}: {line:?}");
                    assert!(!leaves_a_style_open(&line), "{line:?}");
                }
            }
        }
    }

    // ---- a corpus of random sessions ----------------------------------------------------------

    /// A small, fixed, seedable generator — the corpus below must not change when anything else's
    /// randomness does.
    struct Corpus(u64);

    impl Corpus {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
    }

    /// Keys a person or a terminal can send, in the spellings terminals really use — replies,
    /// mouse reports, pastes, torn and runaway sequences and invalid bytes included.
    const CORPUS_KEYS: &[&[u8]] = &[
        b"\x1b[A",
        b"\x1b[B",
        b"\x1b[C",
        b"\x1b[D",
        b"\x1b[1;2A",
        b"\x1b[1;2B",
        b"\x1b[1;2C",
        b"\x1b[1;2D",
        b"\x1b[5~",
        b"\x1b[6~",
        b" ",
        b"\r",
        b"\x7f",
        b"\x1b[3~",
        b"b",
        b"i",
        b"[",
        b"]",
        b"c",
        b"x",
        b"B",
        b"\t",
        b"\x1b[Z",
        b"\x1b[15~",
        b"\x1b[17~",
        b"\x1b[17;2~",
        b"\x1bOQ",
        b"\x1a",
        b"\x19",
        b"\x13",
        b"\x18",
        b"\x03",
        b"\x1b[32u",
        b"\x1b[32;1:2u",
        b"\x1b[32;1:3u",
        b"\x1b[127u",
        b"\x1b[127;1:3u",
        b"\x1b[13u",
        b"\x1b[13;1:3u",
        b"\x1b[3;1:3~",
        b"\x1b[122;6u",
        b"\x1b[57441u",
        b"\x1b[<0;5;5M",
        b"\x1b]0;t\x07",
        b"\x1b[?62;22c",
        b"\x1b[?27u",
        b"\x1b",
        b"\x1b[",
        b"\x1b[99999999999999999999A",
        b"\x00",
        b"\xff",
        "\u{e9}".as_bytes(),
        "\u{6f22}".as_bytes(),
        b"\x1b[200~p\x1b[201~",
        // Typing on the dial: a number, a hex colour, kitty's Shift+3 for `#`, and Tab.
        b"#",
        b"7",
        b"0",
        b"e",
        b"\x1b[51;2;35u",
        b"\t",
    ];

    /// A fixed corpus of random sessions — random art, palettes, key streams and terminal sizes,
    /// from zero columns and rows up — with every frame checked against the promises a frame
    /// makes: never taller or wider than the terminal, never a line left with a style on, and,
    /// while the window follows it, the cursor on screen.
    ///
    /// A CORPUS, NOT A FUZZER. The same seeds give the same inputs only while this generator and
    /// these tables stay as they are, so a green run is a guarantee about these cases and no
    /// others. When it finds something, check the failing input in as a test of its own, rather
    /// than trusting the seed to reproduce it after the next change here.
    #[test]
    fn a_corpus_of_random_sessions_keeps_every_promise_a_frame_makes() {
        with_colour();
        const GLYPHS: &[char] = &['.', '#', ' ', 'a', '\u{28ff}', '\u{2588}', '|', '\u{e9}'];
        for seed in 1..=40u64 {
            let mut rng = Corpus(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
            let (rows, cols) = (1 + rng.below(30), 1 + rng.below(70));
            let art: Vec<String> = (0..rows)
                .map(|_| {
                    (0..1 + rng.below(cols)).map(|_| GLYPHS[rng.below(GLYPHS.len())]).collect()
                })
                .collect();
            let mut palette = Palette::new();
            for n in 0..rng.below(5) {
                // Colours of their own and terminal palette slots alike, as a file can hold.
                let color = match rng.below(3) {
                    0 => Ink::Slot(rng.below(256) as u8),
                    _ => Rgb::new(rng.below(256) as u8, rng.below(256) as u8, n as u8 * 40).into(),
                };
                let _ = palette.push(format!("colour {n}"), color);
            }
            let canvas = Canvas::from_text(&art.join("\n")).expect("the glyphs are all valid");
            let mut picker =
                Picker::new(canvas).with_palette(palette).with_rng(Rng::from_seed(seed));
            picker.set_hold_keys(rng.below(2) == 0);
            picker.set_shift_on_ctrl(rng.below(2) == 0);
            let wide = rng.below(2) == 0;
            picker.set_wide_ambiguous(wide);
            // Half the sessions over the memory line, so the red line is swept with the rest.
            let warns = rng.below(2) == 0;
            picker.set_memory_warning_above(if warns { 0 } else { usize::MAX });
            let mut decoder = crate::keys::Decoder::new();
            for step in 0..80 {
                let mut decoded = decoder.feed(CORPUS_KEYS[rng.below(CORPUS_KEYS.len())]);
                if rng.below(3) == 0 {
                    decoded.extend(decoder.flush());
                }
                for key in decoded.into_iter().filter_map(|d| match d {
                    crate::keys::Decoded::Key(key) => Some(key),
                    _ => None,
                }) {
                    apply(&mut picker, key);
                }
                let (width, height) = (rng.below(120), rng.below(60));
                picker.set_view(picker.scrolled(width, height));
                let lines = render(&picker, width, height);
                let at = format!("seed {seed}, step {step}, {width}x{height}");
                assert!(lines.len() <= height, "{at}: {} lines", lines.len());
                for line in &lines {
                    let measured = text_width(line, wide);
                    assert!(measured <= width, "{at}: {measured} columns: {line:?}");
                    assert!(!leaves_a_style_open(line), "{at}: a style left on: {line:?}");
                }
                // The mark survives a cut only with room for it and the `…` both.
                let mark_room = GUTTER + char_width(CLIPPED, wide);
                if picker.view().follow && body_rows(height, warns) > 0 && width >= mark_room {
                    assert_eq!(marked(&lines).len(), 1, "{at}: the cursor is on screen");
                }
            }
        }
    }

    // ---- what keys do -----------------------------------------------------------------------

    /// `[+]` opens the dial on a colour; Enter there adds it and, without another keystroke, hands
    /// it over to be named: the cursor moves onto the new row and its generated name is the text
    /// being typed.
    #[test]
    fn enter_on_the_button_adds_a_colour_and_asks_for_its_name() {
        let mut picker = picker(0, "ab");
        assert_eq!(picker.focus(), Focus::Add, "with no swatches the button is the top row");
        assert_eq!(apply(&mut picker, ENTER), Action::Redraw);
        assert!(picker.dial().is_some(), "the dial is open");
        assert!(picker.palette().is_empty(), "and nothing is added until its colour is kept");
        assert_eq!(apply(&mut picker, ENTER), Action::Redraw);
        assert_eq!(picker.dial(), None);
        assert_eq!(picker.palette().len(), 1);
        assert_eq!(picker.focus(), Focus::Swatch { at: 0 }, "the cursor is on the new swatch");
        assert_eq!(picker.editing(), Some("colour 1"), "its name is ready to be edited");
        assert_eq!(apply(&mut picker, ENTER), Action::Redraw, "enter keeps the name");
        assert_eq!(picker.editing(), None);
        assert_eq!(picker.palette().at(0).unwrap().label(), "colour 1");
    }

    #[test]
    fn every_press_of_the_button_adds_another_distinct_colour() {
        let mut picker = picker(0, "ab");
        for _ in 0..8 {
            press(&mut picker, &[ENTER, ENTER, ENTER]); // the dial, its colour kept, and the name
            picker.set_focus(Focus::Add);
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
        assert_eq!(apply(&mut picker, SPACE), Action::Redraw, "space picks the brush");
        assert_eq!(picker.brush(), Some("colour 1"));
        picker.set_focus(Focus::Cell { x: 0, y: 0 });
        assert_eq!(apply(&mut picker, ENTER), Action::Redraw, "enter paints with it");
        assert_eq!(inks(&picker, 0)[0], Some(picker.palette().at(0).unwrap().color()));
        assert_eq!(picker.pen(), Pen::Up, "a tap: where releases cannot be heard, nothing latches");
    }

    /// The core loop of the tool: pick a swatch, walk to a cell, put the colour down.
    #[test]
    fn picking_a_swatch_then_confirming_on_a_cell_paints_it() {
        let mut picker = picker(2, "ab\ncd");
        let second = picker.palette().at(1).unwrap().color();
        picker.set_focus(Focus::Swatch { at: 1 });
        press(&mut picker, &[ENTER]);
        assert_eq!(picker.brush(), Some("colour 2"));

        // Down past [+], then into the art, then right once.
        press(&mut picker, &[DOWN, DOWN, RIGHT]);
        assert_eq!(picker.focus(), Focus::Cell { x: 1, y: 0 });
        assert_eq!(apply(&mut picker, ENTER), Action::Redraw);
        assert_eq!(inks(&picker, 0), [None, Some(second)], "only the cell under the cursor");
        assert!(picker.is_dirty());
    }

    /// A pen with nothing to paint stays up, and the toggle that put a pen down lifts it.
    #[test]
    fn the_pen_needs_a_brush_and_the_toggle_lifts_what_it_latched() {
        let mut picker = picker(1, "ab");
        picker.set_focus(Focus::Cell { x: 0, y: 0 });
        assert_eq!(apply(&mut picker, ENTER), Action::Ignored, "no brush picked yet");
        assert_eq!(apply(&mut picker, TOGGLE), Action::Ignored, "nor for the toggle");
        assert_eq!(picker.pen(), Pen::Up);
        assert!(!picker.is_dirty(), "and nothing was recorded");

        let mut picker = ready_to_paint("ab");
        assert_eq!(apply(&mut picker, TOGGLE), Action::Redraw, "down, and painted");
        assert_eq!(picker.pen(), Pen::Painting);
        assert_eq!(apply(&mut picker, TOGGLE), Action::Redraw, "up again");
        assert_eq!(picker.pen(), Pen::Up);
        assert!(inks(&picker, 0)[0].is_some(), "the paint stays");
    }

    /// The toggle drags on every terminal: down, then every cell the cursor lands on takes the
    /// colour until it is pressed again.
    #[test]
    fn the_toggled_pen_paints_every_cell_it_is_dragged_over() {
        let mut picker = ready_to_paint("abcd\nefgh");
        let ink = picker.palette().at(0).unwrap().color();
        press(&mut picker, &[TOGGLE, RIGHT, RIGHT, RIGHT]);
        assert_eq!(inks(&picker, 0), vec![Some(ink); 4], "the whole row, in one drag");
        press(&mut picker, &[DOWN, LEFT]);
        assert_eq!(inks(&picker, 1), [None, None, Some(ink), Some(ink)], "and round the corner");
    }

    #[test]
    fn with_the_pen_up_moving_paints_nothing() {
        let mut picker = ready_to_paint("abc");
        press(&mut picker, &[ENTER, ENTER]); // down, up
        press(&mut picker, &[RIGHT, RIGHT]);
        assert_eq!(inks(&picker, 0)[1..], [None, None]);
    }

    #[test]
    fn leaving_the_canvas_lifts_the_pen() {
        let mut picker = ready_to_paint("ab");
        press(&mut picker, &[TOGGLE]);
        assert_eq!(picker.pen(), Pen::Painting);
        press(&mut picker, &[UP]);
        assert_eq!(picker.focus(), Focus::Add);
        assert_eq!(picker.pen(), Pen::Up, "a pen off the canvas has nothing to draw on");
        press(&mut picker, &[DOWN, RIGHT]);
        assert_eq!(inks(&picker, 0)[1], None, "and coming back does not resume painting");
    }

    /// Delete is the erasing toggle: down on one press, up on the next.
    #[test]
    fn the_erasing_toggle_clears_every_cell_it_is_dragged_over() {
        let mut picker = ready_to_paint("abcd");
        let ink = picker.palette().at(0).unwrap().color();
        press(&mut picker, &[TOGGLE, RIGHT, RIGHT, RIGHT]);
        assert_eq!(inks(&picker, 0), vec![Some(ink); 4]);

        assert_eq!(apply(&mut picker, DELETE), Action::Redraw, "erasing toggle down");
        assert_eq!(picker.pen(), Pen::Erasing, "which replaced the painting pen");
        press(&mut picker, &[LEFT, LEFT]);
        assert_eq!(inks(&picker, 0), [Some(ink), None, None, None], "three erased on the way back");
        assert_eq!(apply(&mut picker, DELETE), Action::Redraw, "and delete lifts it");
        assert_eq!(picker.pen(), Pen::Up);
        picker.set_focus(Focus::Add);
        assert_eq!(apply(&mut picker, DELETE), Action::Ignored, "not on a cell");
    }

    /// One drag, one undo — a stroke of forty cells that took forty presses to take back would be
    /// the first complaint.
    #[test]
    fn a_drag_is_one_undo_and_one_redo() {
        let mut picker = ready_to_paint("abcd");
        let ink = picker.palette().at(0).unwrap().color();
        press(&mut picker, &[TOGGLE, RIGHT, RIGHT, RIGHT, TOGGLE]);
        assert_eq!(apply(&mut picker, UNDO), Action::Redraw);
        assert_eq!(inks(&picker, 0), vec![None; 4], "the whole stroke went at once");
        assert_eq!(apply(&mut picker, REDO), Action::Redraw);
        assert_eq!(inks(&picker, 0), vec![Some(ink); 4], "and came back at once");
    }

    /// Undoing while a pen is still down closes the stroke first, so the whole of what was just
    /// drawn is what goes — not the last cell of it.
    #[test]
    fn undo_mid_stroke_takes_back_the_whole_stroke_and_lifts_the_pen() {
        let mut picker = ready_to_paint("abcd");
        press(&mut picker, &[TOGGLE, RIGHT, RIGHT]);
        assert_eq!(picker.pen(), Pen::Painting);
        press(&mut picker, &[UNDO]);
        assert_eq!(picker.pen(), Pen::Up);
        assert_eq!(inks(&picker, 0), vec![None; 4]);
    }

    /// A tap is recorded as the plain paint it is, and a stroke over cells that already hold the
    /// colour records only the cells it actually changed.
    #[test]
    fn a_tap_is_a_plain_paint_and_a_stroke_records_only_what_it_changed() {
        let mut picker = ready_to_paint("ab");
        press(&mut picker, &[ENTER]);
        assert!(matches!(picker.history.done.last(), Some(Edit::Paint { .. })));
        assert_eq!(picker.history.done.len(), 1);

        press(&mut picker, &[TOGGLE, RIGHT, LEFT, TOGGLE]);
        // (0,0) was already that colour: only (1,0) is new work.
        assert_eq!(picker.history.done.len(), 2);
        assert!(matches!(picker.history.done.last(), Some(Edit::Paint { x: 1, .. })));
    }

    #[test]
    fn ctrl_s_asks_for_a_save_rather_than_doing_one() {
        let mut picker = picker(1, "ab");
        assert_eq!(apply(&mut picker, SAVE), Action::Save);
    }

    /// `RawMode` turns off signal generation, so Ctrl+C arrives as a keystroke and nothing else
    /// will ever act on it. It interrupts at once, unsaved work or not — and asks the caller to
    /// salvage rather than argue.
    #[test]
    fn ctrl_c_interrupts_at_once_even_with_unsaved_work() {
        let mut picker = ready_to_paint("ab");
        press(&mut picker, &[ENTER]);
        assert!(picker.is_dirty());
        assert_eq!(apply(&mut picker, INTERRUPT), Action::Interrupt);
        assert_eq!(picker.pen(), Pen::Up, "the stroke is closed on the way out");
    }

    #[test]
    fn ctrl_c_gives_up_a_half_typed_name_and_interrupts() {
        let mut picker = picker(0, "ab");
        press(&mut picker, &[ENTER, ENTER, ch('z'), ch('z')]);
        assert_eq!(picker.editing(), Some("colour 1zz"));
        assert_eq!(apply(&mut picker, INTERRUPT), Action::Interrupt);
        assert_eq!(picker.editing(), None);
        assert_eq!(picker.palette().at(0).unwrap().label(), "colour 1", "the half-name is gone");
    }

    #[test]
    fn esc_and_ctrl_x_close_a_clean_picker_at_once() {
        for key in [ESC, CLOSE] {
            let mut picker = picker(1, "ab");
            let named = format!("{key:?}");
            assert_eq!(apply(&mut picker, key), Action::Close, "{named}");
        }
    }

    /// Work that has not reached the file is not thrown away on one keystroke: the first close
    /// warns, the second discards, and anything in between withdraws the warning.
    #[test]
    fn closing_with_unsaved_work_warns_first_and_any_other_key_withdraws_it() {
        let mut dirty = ready_to_paint("ab");
        press(&mut dirty, &[ENTER]);
        assert_eq!(apply(&mut dirty, ESC), Action::Redraw, "warned, not closed");
        assert!(dirty.notice().is_some_and(|n| n.contains("unsaved")), "{:?}", dirty.notice());
        assert_eq!(apply(&mut dirty, ESC), Action::Close, "the second press goes through");

        // The same again, but with a key in between: the warning is withdrawn and the next
        // close request warns afresh.
        let mut again = ready_to_paint("ab");
        press(&mut again, &[ENTER]);
        apply(&mut again, ESC);
        assert_eq!(
            apply(&mut again, ch('q')),
            Action::Redraw,
            "an otherwise-ignored key repaints, because it took the warning off the screen"
        );
        assert_eq!(again.notice(), None);
        assert_eq!(apply(&mut again, CLOSE), Action::Redraw, "warns again");
    }

    #[test]
    fn the_arrow_keys_move_the_cursor() {
        let mut picker = picker(2, "ab\ncd");
        assert_eq!(picker.focus(), Focus::Swatch { at: 0 });
        assert_eq!(apply(&mut picker, DOWN), Action::Redraw);
        assert_eq!(picker.focus(), Focus::Swatch { at: 1 });
        apply(&mut picker, DOWN);
        assert_eq!(picker.focus(), Focus::Add);
        apply(&mut picker, DOWN);
        assert_eq!(picker.focus(), Focus::Cell { x: 0, y: 0 });
        assert_eq!(apply(&mut picker, RIGHT), Action::Redraw);
        assert_eq!(picker.focus(), Focus::Cell { x: 1, y: 0 });
        apply(&mut picker, UP);
        assert_eq!(picker.focus(), Focus::Add, "and back out of the art");
    }

    #[test]
    fn tab_and_shift_tab_are_down_and_up() {
        let mut picker = picker(2, "ab");
        apply(&mut picker, TAB);
        assert_eq!(picker.focus(), Focus::Swatch { at: 1 });
        apply(&mut picker, BACKTAB);
        assert_eq!(picker.focus(), Focus::Swatch { at: 0 });
    }

    /// An edge refuses the move, and refusing is not a reason to redraw an identical frame.
    #[test]
    fn a_key_that_moves_nothing_is_ignored_rather_than_redrawn() {
        let mut picker = picker(1, "ab");
        assert_eq!(apply(&mut picker, UP), Action::Ignored, "top edge");
        assert_eq!(apply(&mut picker, LEFT), Action::Ignored, "one column");
        assert_eq!(picker.focus(), Focus::Swatch { at: 0 });
    }

    /// Terminals emit more than keystrokes, and every junk event that redraws is a wasted frame.
    #[test]
    fn a_key_with_no_meaning_here_is_ignored() {
        let mut picker = picker(1, "ab");
        picker.set_focus(Focus::Add);
        for key in [
            ch('q'),
            KeyEvent::press(KeyCode::PageUp),
            KeyEvent::press(KeyCode::Insert),
            KeyEvent::press(KeyCode::Unknown),
            KeyEvent::press(KeyCode::Home),
        ] {
            let named = format!("{key:?}");
            assert_eq!(apply(&mut picker, key), Action::Ignored, "{named}");
        }
    }

    // ---- function keys, as console delivers them ---------------------------------------------

    /// F2 as three different terminals spell it, from the bytes up — `console` split two of these
    /// into an unknown escape plus a stray typed letter. Driven through the real decoder, so it
    /// is the wiring that is checked, not a hand-built event.
    #[test]
    fn f2_is_recognised_in_every_spelling_a_terminal_sends() {
        for spelling in [&b"\x1bOQ"[..], b"\x1b[12~", b"\x1b[[B", b"\x1b[12;1:1~"] {
            let mut picker = picker(1, "ab");
            let colour = picker.palette().at(0).unwrap().color().approximate();
            type_bytes(&mut picker, spelling);
            let dial = picker.dial().unwrap_or_else(|| panic!("{spelling:?} opened no dial"));
            assert_eq!(dial.start(), colour, "{spelling:?} starts from the swatch's colour");
        }
    }

    /// The letter or tilde that trails an F-key must never come out as typing. Checked in the one
    /// place a stray character would do damage: a name being edited.
    #[test]
    fn other_function_keys_are_swallowed_rather_than_half_typed() {
        let mut picker = picker(0, "ab");
        press(&mut picker, &[ENTER, ENTER]); // naming "colour 1"
        for spelling in
            [&b"\x1bOP"[..], b"\x1bOR", b"\x1bOS", b"\x1b[11~", b"\x1b[14~", b"\x1b[24~"]
        {
            type_bytes(&mut picker, spelling);
            assert_eq!(picker.editing(), Some("colour 1"), "{spelling:?} typed something");
        }
    }

    /// An arrow whose bytes straddle two reads is still an arrow — not a bare Escape (which here
    /// would be a close request) followed by `[` and `B` typed as text.
    #[test]
    fn an_arrow_split_across_reads_is_still_an_arrow() {
        let mut picker = picker(2, "ab");
        let mut decoder = crate::keys::Decoder::new();
        assert!(decoder.feed(b"\x1b").is_empty());
        for d in decoder.feed(b"[B") {
            if let crate::keys::Decoded::Key(key) = d {
                apply(&mut picker, key);
            }
        }
        assert_eq!(picker.focus(), Focus::Swatch { at: 1 }, "the arrow moved");
        assert_eq!(picker.notice(), None, "and nothing thought Escape was pressed");
    }

    #[test]
    fn f2_anywhere_but_a_swatch_does_nothing() {
        let mut picker = picker(1, "ab");
        for focus in [Focus::Add, Focus::Cell { x: 0, y: 0 }] {
            picker.set_focus(focus);
            assert_eq!(press(&mut picker, &[F2]), Action::Ignored, "{focus:?}");
            assert_eq!(picker.editing(), None);
        }
    }

    // ---- holding: presses, repeats and releases ---------------------------------------------

    fn release(code: KeyCode) -> KeyEvent {
        KeyEvent::release(code)
    }

    fn repeat(code: KeyCode) -> KeyEvent {
        KeyEvent::repeat(code)
    }

    /// The reported bug, as the developer then resolved it: Backspace is the erasing HOLD, so
    /// letting it go ends the stroke. (Delete became the erasing toggle — see
    /// `delete_keeps_erasing_after_it_is_let_go`.)
    #[test]
    fn letting_go_of_backspace_stops_erasing() {
        let mut picker = ready_to_hold("a\nb\nc\nd");
        let ink = picker.palette().at(0).unwrap().color();
        press(&mut picker, &[SPACE, DOWN, DOWN, DOWN, release(KeyCode::Char(' '))]);
        assert!((0..4).all(|y| inks(&picker, y)[0] == Some(ink)), "a column painted");

        picker.set_focus(Focus::Cell { x: 0, y: 0 });
        press(&mut picker, &[BACKSPACE, DOWN]);
        assert_eq!(picker.pen(), Pen::Erasing);
        press(&mut picker, &[release(KeyCode::Backspace)]);
        assert_eq!(picker.pen(), Pen::Up, "released: the pen is up");
        press(&mut picker, &[DOWN, DOWN]);
        let column: Vec<_> = (0..4).map(|y| inks(&picker, y)[0]).collect();
        assert_eq!(column, [None, None, Some(ink), Some(ink)], "only what was passed while held");
    }

    /// …and Delete, being a toggle, keeps erasing after it is released, until pressed again. A
    /// release of a toggle key means nothing — that is what makes it a toggle.
    #[test]
    fn delete_keeps_erasing_after_it_is_let_go() {
        let mut picker = ready_to_hold("abcd");
        press(&mut picker, &[TOGGLE, RIGHT, RIGHT, RIGHT, TOGGLE]);
        picker.set_focus(Focus::Cell { x: 0, y: 0 });
        press(&mut picker, &[DELETE, release(KeyCode::Delete), RIGHT, RIGHT]);
        assert_eq!(picker.pen(), Pen::Erasing, "still down");
        assert_eq!(inks(&picker, 0)[..3], [None, None, None]);
    }

    /// The reported bug: Space held, then Right held — and the colour stopped part-way. A held key
    /// auto-repeats, and when every repeat was a press, each one toggled the pen. Repeats are now
    /// told apart and ignored, so holding means holding.
    #[test]
    fn a_held_space_repeating_never_toggles_the_pen() {
        let mut picker = ready_to_hold("abcdef");
        let ink = picker.palette().at(0).unwrap().color();
        let held = repeat(KeyCode::Char(' '));
        press(&mut picker, &[SPACE, held, held, held, RIGHT, held, RIGHT, RIGHT, held, RIGHT]);
        assert_eq!(picker.pen(), Pen::Painting, "still down after every repeat");
        assert_eq!(inks(&picker, 0), [Some(ink), Some(ink), Some(ink), Some(ink), Some(ink), None]);
        press(&mut picker, &[release(KeyCode::Char(' ')), RIGHT]);
        assert_eq!(inks(&picker, 0)[5], None, "and after the release, moving paints nothing");
    }

    #[test]
    fn releasing_space_or_enter_lifts_the_painting_pen() {
        for key in [KeyCode::Char(' '), KeyCode::Enter] {
            let mut picker = ready_to_hold("abc");
            press(&mut picker, &[KeyEvent::press(key), RIGHT]);
            assert_eq!(apply(&mut picker, release(key)), Action::Redraw, "{key:?}");
            assert_eq!(picker.pen(), Pen::Up, "{key:?}");
        }
    }

    /// A release lifts only the pen its key holds — letting go of an arrow mid-drag is not the
    /// end of the stroke.
    #[test]
    fn a_release_of_any_other_key_leaves_the_pen_down() {
        let mut picker = ready_to_hold("abc");
        press(&mut picker, &[SPACE, RIGHT]);
        for other in [KeyCode::Right, KeyCode::Delete, KeyCode::Char('q'), KeyCode::Escape] {
            assert_eq!(apply(&mut picker, release(other)), Action::Ignored, "{other:?}");
            assert_eq!(picker.pen(), Pen::Painting, "{other:?}");
        }
    }

    /// The degradation rule: a terminal can answer the protocol query and still drop releases
    /// (tmux). A second press lifts, so a lost release cannot leave the pen stuck down.
    #[test]
    fn a_second_press_lifts_the_pen_even_without_a_release() {
        let mut picker = ready_to_paint("abc");
        press(&mut picker, &[SPACE, RIGHT, SPACE]);
        assert_eq!(picker.pen(), Pen::Up);
        press(&mut picker, &[RIGHT]);
        assert_eq!(inks(&picker, 0)[2], None);
    }

    /// The trap designed around: under the protocol, releasing Esc is an event of its own. If it
    /// counted as a keystroke it would disarm the warning its own press had just armed, and a
    /// second Esc could never close.
    #[test]
    fn releasing_esc_does_not_withdraw_the_close_warning() {
        let mut dirty = ready_to_paint("ab");
        press(&mut dirty, &[SPACE, release(KeyCode::Char(' '))]);
        assert_eq!(apply(&mut dirty, ESC), Action::Redraw, "warned");
        assert_eq!(apply(&mut dirty, release(KeyCode::Escape)), Action::Ignored);
        assert!(dirty.notice().is_some(), "the warning is still on screen");
        assert_eq!(apply(&mut dirty, ESC), Action::Close, "and the second Esc goes through");
    }

    /// With every key an escape, Shift and Ctrl report their own presses and releases. They must
    /// neither end a hold nor count as a keystroke.
    #[test]
    fn lone_modifier_keys_are_inert() {
        let mut picker = ready_to_hold("abc");
        press(&mut picker, &[SPACE]);
        picker.notice = Some("news".into());
        let shift = KeyCode::Modifier;
        assert_eq!(apply(&mut picker, KeyEvent::press(shift)), Action::Ignored);
        assert_eq!(apply(&mut picker, release(shift)), Action::Ignored);
        assert_eq!(picker.pen(), Pen::Painting);
        assert_eq!(picker.notice(), Some("news"), "not a keystroke, so the notice stays");
    }

    /// A key held down must not machine-gun what it does once: a held Enter on `[+]` adds one
    /// swatch, a held Ctrl+S is one save.
    #[test]
    fn repeats_of_keys_that_act_once_do_nothing() {
        let mut picker = picker(0, "ab");
        press(&mut picker, &[ENTER, ENTER, ESC]); // add one, keep its colour and its name
        picker.set_focus(Focus::Add);
        assert_eq!(apply(&mut picker, repeat(KeyCode::Enter)), Action::Ignored);
        assert_eq!(picker.palette().len(), 1);
        let held_save = KeyEvent { kind: KeyKind::Repeat, ..SAVE };
        assert_eq!(apply(&mut picker, held_save), Action::Ignored);
        assert_eq!(apply(&mut picker, KeyEvent { kind: KeyKind::Repeat, ..F2 }), Action::Ignored);
    }

    /// …but a held Ctrl+Z walks back through the history, as it does in any editor.
    #[test]
    fn holding_undo_keeps_undoing() {
        let mut picker = ready_to_paint("abc");
        for _ in 0..3 {
            press(&mut picker, &[SPACE, RIGHT]);
            press(&mut picker, &[release(KeyCode::Char(' '))]);
        }
        let held_undo = KeyEvent { kind: KeyKind::Repeat, ..UNDO };
        press(&mut picker, &[UNDO, held_undo, held_undo]);
        assert!(inks(&picker, 0).iter().all(Option::is_none), "three strokes, three undos");
    }

    /// Redo is Ctrl+Shift+Z where the terminal reports the Shift — here in the kitty protocol's
    /// own bytes — and Ctrl+Y wherever it does not; a classic terminal's Ctrl+Shift+Z is the very
    /// byte Ctrl+Z sends, so there it can only undo. The hint names whichever this terminal has.
    #[test]
    fn ctrl_shift_z_redoes_where_the_terminal_can_tell_it_from_ctrl_z() {
        let mut picker = ready_to_paint("abc");
        press(&mut picker, &[SPACE]);
        let painted = inks(&picker, 0);
        press(&mut picker, &[UNDO]);
        assert_eq!(type_bytes(&mut picker, b"\x1b[122;6u"), Action::Redraw, "kitty's ctrl+shift+z");
        assert_eq!(inks(&picker, 0), painted, "redone");
        type_bytes(&mut picker, b"\x1a");
        assert!(inks(&picker, 0).iter().all(Option::is_none), "the classic byte is ctrl+z: undo");
        assert_eq!(type_bytes(&mut picker, b"\x19"), Action::Redraw, "ctrl+y still redoes");
        assert_eq!(inks(&picker, 0), painted);

        assert!(hints(&picker).contains("^Y redo"), "{}", hints(&picker));
        picker.set_shift_on_ctrl(true);
        assert!(hints(&picker).contains("shift+^Z redo"), "{}", hints(&picker));
    }

    /// Typing reads what a key TYPED, not which key it was: Shift+A under the protocol is the key
    /// `a` with the text `A`. A chord or a release types nothing.
    #[test]
    fn typing_a_name_reads_the_text_a_key_produced() {
        let mut naming = picker(0, "ab");
        press(&mut naming, &[ENTER, ENTER]);
        press(&mut naming, &vec![BACKSPACE; "colour 1".len()]);
        // Shift+A with its text, then b, then b's release, then Ctrl+Q — all as kitty sends them.
        type_bytes(&mut naming, b"\x1b[97;2;65u\x1b[98u\x1b[98;1:3u\x1b[113;5u");
        assert_eq!(naming.editing(), Some("Ab"), "the release and the chord typed nothing");
    }

    // ---- hold keys, toggle keys, and the tap -------------------------------------------------

    /// Where releases cannot be heard, Space is a TAP: the cell under the cursor, once. It never
    /// latches — a latching Space is exactly what the developer asked to be rid of.
    #[test]
    fn space_is_a_tap_where_releases_cannot_be_heard() {
        let mut picker = ready_to_paint("abc");
        let ink = picker.palette().at(0).unwrap().color();
        press(&mut picker, &[SPACE]);
        assert_eq!(inks(&picker, 0)[0], Some(ink), "the cell under the cursor");
        assert_eq!(picker.pen(), Pen::Up, "and nothing latched");
        press(&mut picker, &[RIGHT, RIGHT]);
        assert_eq!(inks(&picker, 0)[1..], [None, None], "so moving paints nothing");
        press(&mut picker, &[SPACE, SPACE, SPACE]);
        assert_eq!(picker.pen(), Pen::Up, "however many presses — no parity, no toggle");
        assert_eq!(inks(&picker, 0)[2], Some(ink));
    }

    #[test]
    fn backspace_is_a_tap_too() {
        let mut picker = ready_to_paint("abc");
        press(&mut picker, &[TOGGLE, RIGHT, RIGHT, TOGGLE]);
        press(&mut picker, &[BACKSPACE]);
        assert_eq!(inks(&picker, 0)[2], None, "the cell under the cursor, erased");
        assert_eq!(picker.pen(), Pen::Up);
        press(&mut picker, &[LEFT]);
        assert!(inks(&picker, 0)[1].is_some(), "and nothing more");
    }

    /// The toggles need nothing from the terminal, so they drag the same everywhere.
    #[test]
    fn b_toggles_painting_whether_or_not_releases_are_heard() {
        for holds in [false, true] {
            let mut picker = ready_to_paint("abc");
            picker.set_hold_keys(holds);
            let ink = picker.palette().at(0).unwrap().color();
            press(&mut picker, &[TOGGLE, release(KeyCode::Char('b')), RIGHT, RIGHT, TOGGLE, LEFT]);
            assert_eq!(inks(&picker, 0), [Some(ink); 3], "holds={holds}");
            assert_eq!(picker.pen(), Pen::Up, "holds={holds}");
        }
    }

    /// The self-heal: a second press of the key already holding the pen means its release never
    /// came. The picker stops asking for holds and says so — a stuck pen would be worse.
    #[test]
    fn a_missed_release_turns_holding_into_tapping() {
        let mut picker = ready_to_hold("abcd");
        press(&mut picker, &[SPACE, RIGHT]);
        assert!(picker.pen_is_held());
        // No release in between: it was lost. The cell is already painted, so the frame changes
        // only by the lifted pen and the notice — and it must still be drawn, or the next key
        // clears the notice before anyone has seen it.
        assert_eq!(press(&mut picker, &[SPACE]), Action::Redraw, "the change reaches the screen");
        assert!(!picker.hold_keys, "releases are no longer trusted");
        assert_eq!(picker.pen(), Pen::Up);
        assert!(picker.notice().is_some_and(|n| n.contains("releases are not arriving")));
        press(&mut picker, &[RIGHT]);
        assert_eq!(inks(&picker, 0)[2], None, "and nothing is left painting behind the cursor");
    }

    /// Space held, then Backspace pressed on top of it: the last key down is in charge. Letting
    /// it go hands the pen back to Space — without repainting the cell it just erased — and
    /// letting Space go lifts the pen.
    #[test]
    fn with_two_hold_keys_down_the_last_pressed_is_in_charge() {
        let mut picker = ready_to_hold("abcde");
        let ink = Some(picker.palette().at(0).unwrap().color());
        press(&mut picker, &[SPACE, RIGHT]); // (0,0) and (1,0) painted
        press(&mut picker, &[BACKSPACE]); // on top, and it acts at once: (1,0) erased
        assert_eq!(picker.pen(), Pen::Erasing);
        press(&mut picker, &[RIGHT]); // (2,0) erased, which it already was
        assert_eq!(apply(&mut picker, release(KeyCode::Backspace)), Action::Redraw);
        assert_eq!(picker.pen(), Pen::Painting, "space is still down: painting again");
        assert!(picker.pen_is_held());
        assert_eq!(inks(&picker, 0)[2], None, "handed back, not pressed: nothing repainted here");
        press(&mut picker, &[RIGHT]); // (3,0) painted
        press(&mut picker, &[release(KeyCode::Char(' ')), RIGHT]); // lifted: (4,0) untouched
        assert_eq!(inks(&picker, 0), [ink, None, None, ink, None]);
        assert_eq!(picker.pen(), Pen::Up);
    }

    /// Which comes up first does not matter: the key underneath going up leaves the one on top
    /// in charge.
    #[test]
    fn releasing_the_key_underneath_leaves_the_one_on_top_in_charge() {
        let mut picker = ready_to_hold("abcd");
        press(&mut picker, &[SPACE, BACKSPACE, release(KeyCode::Char(' '))]);
        assert_eq!(picker.pen(), Pen::Erasing, "backspace is still down");
        assert!(picker.pen_is_held());
        press(&mut picker, &[release(KeyCode::Backspace)]);
        assert_eq!(picker.pen(), Pen::Up);
    }

    /// Each hand-over between held keys is its own stroke, so undo takes them back one at a time.
    #[test]
    fn each_hand_over_between_held_keys_is_its_own_undo() {
        let mut picker = ready_to_hold("abcd");
        let ink = Some(picker.palette().at(0).unwrap().color());
        press(&mut picker, &[SPACE, RIGHT, RIGHT]); // (0..=2, 0) painted
        press(&mut picker, &[BACKSPACE]); // (2,0) erased
        press(&mut picker, &[release(KeyCode::Backspace), release(KeyCode::Char(' '))]);
        assert_eq!(inks(&picker, 0), [ink, ink, None, None]);
        press(&mut picker, &[UNDO]);
        assert_eq!(inks(&picker, 0), [ink, ink, ink, None], "the erase, alone");
        press(&mut picker, &[UNDO]);
        assert_eq!(inks(&picker, 0), [None; 4], "then the painting stroke");
    }

    /// Two keys that both paint — Space and Enter — are one stroke, however they overlap.
    #[test]
    fn two_painting_keys_held_together_are_one_stroke() {
        let mut picker = ready_to_hold("abcd");
        press(&mut picker, &[SPACE, RIGHT, ENTER, RIGHT, release(KeyCode::Char(' ')), RIGHT]);
        assert!(picker.pen_is_held(), "enter still holds");
        assert!(inks(&picker, 0).iter().all(Option::is_some), "painted the whole way");
        press(&mut picker, &[release(KeyCode::Enter)]);
        assert_eq!(picker.pen(), Pen::Up);
        press(&mut picker, &[UNDO]);
        assert!(inks(&picker, 0).iter().all(Option::is_none), "one undo for all of it");
    }

    /// A second press of a key at the bottom of the stack is the same lost release as ever — the
    /// key on top of it does not hide that.
    #[test]
    fn a_missed_release_is_caught_even_under_another_held_key() {
        let mut picker = ready_to_hold("abcd");
        press(&mut picker, &[SPACE, BACKSPACE]);
        press(&mut picker, &[SPACE]); // space's release never came
        assert!(!picker.hold_keys, "releases are no longer trusted");
        assert_eq!(picker.pen(), Pen::Up);
        assert!(picker.held_keys().is_empty(), "and nothing is left holding");
    }

    /// Whichever key went down last is in charge: a hold takes over from a toggle, and a toggle
    /// from a hold — one stroke ends and the next begins.
    #[test]
    fn a_hold_and_a_toggle_take_over_from_each_other() {
        let mut picker = ready_to_hold("abcd");
        press(&mut picker, &[TOGGLE, SPACE]);
        assert!(picker.pen_is_held(), "the hold took over");
        press(&mut picker, &[release(KeyCode::Char(' '))]);
        assert_eq!(picker.pen(), Pen::Up, "and its release ends it — the toggle is gone too");

        press(&mut picker, &[BACKSPACE, TOGGLE]);
        assert_eq!(picker.pen(), Pen::Painting);
        assert!(!picker.pen_is_held(), "the toggle took over from the held eraser");
        press(&mut picker, &[release(KeyCode::Backspace)]);
        assert_eq!(picker.pen(), Pen::Painting, "so letting go of backspace changes nothing now");
    }

    /// Naming comes first: `b` typed into a name is a letter, not a toggle.
    #[test]
    fn b_types_into_a_name_rather_than_toggling() {
        let mut picker = picker(0, "ab");
        press(&mut picker, &[ENTER, ENTER, TOGGLE]);
        assert_eq!(picker.editing(), Some("colour 1b"));
        assert_eq!(picker.pen(), Pen::Up);
    }

    // ---- the pen's size, and picking a colour up -------------------------------------------

    fn grow(picker: &mut Picker, times: usize) {
        for _ in 0..times {
            assert_eq!(apply(picker, ch(']')), Action::Redraw);
        }
    }

    /// `]` and `[` step the pen a cell each way at a time, from the cursor's cell alone up to the
    /// canvas's longer side, and no further either way.
    #[test]
    fn the_brackets_grow_and_shrink_the_pen_within_its_bounds() {
        let mut picker = ready_to_paint("abcde\nfghij");
        assert_eq!(picker.pen_size(), 1);
        assert_eq!(apply(&mut picker, ch('[')), Action::Ignored, "1x1 is the smallest");
        grow(&mut picker, 4);
        assert_eq!(picker.pen_size(), 5, "the longer side of a 5x2 canvas");
        assert_eq!(apply(&mut picker, ch(']')), Action::Ignored, "and that is the largest");
        assert_eq!(apply(&mut picker, ch('[')), Action::Redraw);
        assert_eq!(picker.pen_size(), 4);
        assert!(status_of(&picker).contains("pen 4x4"), "{}", status_of(&picker));
        picker.pen_size = 1;
        assert!(!status_of(&picker).contains("pen "), "a 1x1 pen goes unsaid");
    }

    /// A bigger pen paints a square centred on the cursor — leaning right and down when its size
    /// is even — clipped to the canvas, and a tap of it is ONE undo however many cells it covers.
    #[test]
    fn a_big_pen_paints_a_clipped_square_around_the_cursor_as_one_edit() {
        let mut picker = ready_to_paint(&["abcde"; 5].join("\n"));
        let ink = Some(picker.palette().at(0).unwrap().color());
        picker.set_focus(Focus::Cell { x: 2, y: 2 });
        grow(&mut picker, 2); // 3x3
        press(&mut picker, &[SPACE]);
        for y in 0..5 {
            let expected: Vec<_> =
                (0..5).map(|x| (1..4).contains(&x) && (1..4).contains(&y)).collect();
            let painted: Vec<_> = inks(&picker, y).iter().map(|&i| i == ink).collect();
            assert_eq!(painted, expected, "row {y}");
        }
        assert!(
            matches!(picker.history.done.last(), Some(Edit::Stroke(parts)) if parts.len() == 9)
        );
        press(&mut picker, &[UNDO]);
        assert!((0..5).all(|y| inks(&picker, y).iter().all(Option::is_none)), "all of it, at once");

        picker.set_focus(Focus::Cell { x: 0, y: 0 });
        press(&mut picker, &[SPACE]);
        let painted = (0..5).map(|y| inks(&picker, y).iter().filter(|&&i| i == ink).count());
        assert_eq!(painted.sum::<usize>(), 4, "at the corner, the square is clipped to 2x2");

        let mut even = ready_to_paint(&["abcde"; 5].join("\n"));
        even.set_focus(Focus::Cell { x: 2, y: 2 });
        grow(&mut even, 1); // 2x2
        assert_eq!(
            even.footprint(),
            Some(Footprint { xs: 2..4, ys: 2..4 }),
            "leans right and down"
        );
    }

    /// Dragging a big pen sweeps its whole width, erasing the same way painting does, and a
    /// pen already down covers its new size the moment it grows.
    #[test]
    fn a_big_pen_drags_and_erases_its_whole_width() {
        let mut picker = ready_to_paint(&["abcdef"; 3].join("\n"));
        let ink = Some(picker.palette().at(0).unwrap().color());
        picker.set_focus(Focus::Cell { x: 1, y: 1 });
        press(&mut picker, &[TOGGLE]); // down, 1x1: only (1,1)
        grow(&mut picker, 2); // 3x3 at once: columns 0-2 of every row
        press(&mut picker, &[RIGHT, RIGHT, TOGGLE]);
        for y in 0..3 {
            assert_eq!(inks(&picker, y), [ink, ink, ink, ink, ink, None], "row {y}");
        }
        press(&mut picker, &[UNDO]);
        assert!(
            (0..3).all(|y| inks(&picker, y).iter().all(Option::is_none)),
            "one stroke, one undo"
        );
        press(&mut picker, &[REDO, DELETE]); // erase 3x3 around (3,1)
        for y in 0..3 {
            assert_eq!(inks(&picker, y), [ink, ink, None, None, None, None], "row {y}");
        }
    }

    /// A painted stretch is drawn as ONE styled span, whatever its length — the reason a heavily
    /// painted picture costs about what a plain one does to redraw.
    #[test]
    fn a_painted_run_is_one_span_however_long() {
        with_colour();
        let mut picker = ready_to_paint(&["abcdefghij"; 2].join("\n"));
        press(&mut picker, &[TOGGLE]);
        press(&mut picker, &[RIGHT; 9]);
        press(&mut picker, &[TOGGLE, DOWN]); // painted all of row 0; the cursor is on row 1
        let Rgb { r, g, b } = rgb(picker.palette().at(0).unwrap().color());
        let row = line_showing(&picker, BodyLine::Art(0));
        assert_eq!(row.matches(&format!("\x1b[38;2;{r};{g};{b}m")).count(), 1, "{row:?}");
        assert!(console::strip_ansi_codes(&row).contains("abcdefghij"), "every glyph: {row:?}");
    }

    /// The pen's footprint is what the cursor looks like: every cell it will touch is marked, in
    /// the art and on the split canvas alike, and the gutter mark stays on the cursor's own row.
    #[test]
    fn the_cursor_shows_the_whole_footprint() {
        with_colour();
        let mut picker = ready_to_paint(&["abcde"; 4].join("\n"));
        picker.set_focus(Focus::Cell { x: 2, y: 1 });
        grow(&mut picker, 2);
        // One inverted span per row, covering exactly the three glyphs the pen will touch.
        let span = glyph_style(None, true).apply_to("bcd").to_string();
        let row = |y| line_showing(&picker, BodyLine::Art(y));
        assert_eq!(
            (0..4).map(|y| row(y).contains(&span)).collect::<Vec<_>>(),
            [true, true, true, false]
        );
        assert_eq!(
            (0..4).map(|y| row(y).matches("\x1b[7m").count()).collect::<Vec<_>>(),
            [1, 1, 1, 0]
        );
        assert_eq!(marked(&render(&picker, 200, 60)), [body_index(&picker, BodyLine::Art(1))]);
        let split = split(&picker);
        let row = line_showing(&split, BodyLine::Art(0));
        assert_eq!(row.matches(BLOCK_CURSOR).count(), 3, "three + on the canvas row: {row:?}");
    }

    /// `i` takes the brush from the canvas: the swatch holding the colour under the cursor, as
    /// if it had been picked from the palette — and says so when there is nothing to pick up.
    #[test]
    fn i_picks_the_colour_under_the_cursor_as_the_brush() {
        with_colour();
        let mut palette = Palette::new();
        palette.push("red", RED).expect("fresh");
        palette.push("blue", BLUE).expect("fresh");
        let mut picker = Picker::new(Canvas::from_text("ab").expect("valid")).with_palette(palette);
        picker.canvas_mut().cell_mut(0, 0).expect("in bounds").ink = Some(BLUE.into());
        picker.set_focus(Focus::Swatch { at: 0 });
        press(&mut picker, &[ENTER]);
        assert_eq!(picker.brush(), Some("red"));

        picker.set_focus(Focus::Cell { x: 0, y: 0 });
        assert_eq!(apply(&mut picker, ch('i')), Action::Redraw);
        assert_eq!(picker.brush(), Some("blue"), "the swatch that holds the cell's colour");
        assert_eq!(picker.focus(), Focus::Cell { x: 0, y: 0 }, "without leaving the canvas");
        assert_eq!(apply(&mut picker, ch('i')), Action::Ignored, "already the brush");

        picker.set_focus(Focus::Cell { x: 1, y: 0 });
        assert_eq!(apply(&mut picker, ch('i')), Action::Redraw, "the notice must be seen");
        assert_eq!(picker.brush(), Some("blue"), "an empty cell leaves the brush alone");
        assert!(picker.notice().is_some_and(|n| n.contains("no colour")), "{:?}", picker.notice());
        assert!(
            hints(&picker).contains("i pick colour") && hints(&picker).contains("[ ] pen size")
        );
    }

    /// While a name is typed, the new keys are letters like any other.
    #[test]
    fn the_pen_and_picking_keys_type_into_a_name() {
        let mut picker = picker(1, "ab");
        picker.begin_rename(0);
        press(&mut picker, &[ch('i'), ch('['), ch(']'), ch('c')]);
        assert_eq!(picker.editing(), Some("colour 1i[]c"));
        assert_eq!(picker.pen_size(), 1);
    }

    // ---- terminal palette slots, beside colours of their own ------------------------------

    /// A picker over one row of art, with a slot swatch — the terminal's slot `slot` — as its
    /// brush and the cursor on the first cell.
    fn slotted(slot: u8) -> Picker {
        let mut palette = Palette::new();
        palette.push("sea", Ink::Slot(slot)).expect("fresh");
        palette.push("ember", RED).expect("fresh");
        let mut picker = Picker::new(Canvas::from_text("abc").unwrap()).with_palette(palette);
        picker.set_focus(Focus::Swatch { at: 0 });
        press(&mut picker, &[ENTER]);
        picker.set_focus(Focus::Cell { x: 0, y: 0 });
        picker
    }

    /// A slot swatch is drawn through the terminal's own palette — so it looks exactly as this
    /// terminal shows that slot — and says what it is after its label; the sixteen that follow
    /// the theme say that too. A colour of its own says nothing.
    #[test]
    fn a_slot_swatch_draws_as_the_terminals_slot_and_says_so() {
        with_colour();
        let picker = slotted(196);
        let row = line_showing(&picker, BodyLine::Swatch(0));
        assert!(row.contains("\x1b[48;5;196m"), "the terminal's own slot 196: {row:?}");
        assert!(console::strip_ansi_codes(&row).contains("# sea  slot 196"), "{row:?}");
        assert!(!line_showing(&picker, BodyLine::Swatch(1)).contains("slot"), "ember is its own");
        let themed = line_showing(&slotted(3), BodyLine::Swatch(0));
        assert!(console::strip_ansi_codes(&themed).contains("slot 3 (theme)"), "{themed:?}");
    }

    /// Painting with a slot inks the cells with the slot itself, drawn as the terminal's slot;
    /// `i` picks the slot swatch back up from them.
    #[test]
    fn a_slot_paints_as_itself_and_is_picked_back_up() {
        with_colour();
        let mut picker = slotted(196);
        press(&mut picker, &[SPACE]);
        assert_eq!(inks(&picker, 0)[0], Some(Ink::Slot(196)));
        assert!(line_showing(&picker, BodyLine::Art(0)).contains("\x1b[38;5;196m"));

        picker.set_focus(Focus::Swatch { at: 1 });
        press(&mut picker, &[ENTER]);
        assert_eq!(picker.brush(), Some("ember"));
        picker.set_focus(Focus::Cell { x: 0, y: 0 });
        press(&mut picker, &[ch('i')]);
        assert_eq!(picker.brush(), Some("sea"), "the slot swatch, from the cell");
    }

    /// The dial turns colours of their own, so a colour kept from it on a slot swatch makes a
    /// regular one — and says so, since the swatch stops following the terminal. Undo makes it the
    /// slot again.
    #[test]
    fn f2_on_a_slot_swatch_makes_it_a_colour_of_its_own_and_says_so() {
        let mut picker = slotted(196);
        picker.set_focus(Focus::Swatch { at: 0 });
        press(&mut picker, &[F2, UP, ENTER, ESC]);
        let now = picker.palette().get("sea").unwrap().color();
        assert!(matches!(now, Ink::Rgb(_)), "{now:?}");
        press(&mut picker, &[UNDO]);
        assert_eq!(picker.palette().get("sea").unwrap().color(), Ink::Slot(196));
    }

    /// The notice a colour kept from F2's dial leaves on a slot swatch, said while the name is
    /// still being typed.
    #[test]
    fn a_slot_swatch_given_a_colour_of_its_own_is_announced() {
        let mut picker = slotted(21);
        picker.set_focus(Focus::Swatch { at: 0 });
        press(&mut picker, &[F2, UP, ENTER]);
        let notice = picker.notice().expect("a notice");
        assert!(notice.contains("no longer the terminal's slot 21"), "{notice}");
    }

    /// `[+]` only ever adds a colour of its own: a slot's look is the terminal's, and nothing a
    /// person did not ask for should come to depend on it.
    #[test]
    fn the_button_adds_colours_of_their_own_only() {
        let mut picker = picker(0, "ab");
        for _ in 0..20 {
            picker.set_focus(Focus::Add);
            press(&mut picker, &[ENTER, ENTER, ENTER]); // the dial, its colour kept, and the name
        }
        assert_eq!(picker.palette().len(), 20);
        assert!(picker.palette().iter().all(|swatch| matches!(swatch.color(), Ink::Rgb(_))));
    }

    /// On a slot the block cursor chooses black or white by the colour the slot usually is.
    #[test]
    fn the_block_cursor_reads_a_slot_by_its_usual_colour() {
        with_colour();
        let on_white_slot = block_cursor(Ink::Slot(231), 1);
        assert!(on_white_slot.contains("\x1b[38;2;0;0;0m"), "black on slot 231: {on_white_slot:?}");
        assert!(on_white_slot.contains("\x1b[48;5;231m"), "on the slot itself: {on_white_slot:?}");
    }

    // ---- the colour dial ----------------------------------------------------------------------

    const PAGE_UP: KeyEvent = KeyEvent::press(KeyCode::PageUp);
    const PAGE_DOWN: KeyEvent = KeyEvent::press(KeyCode::PageDown);

    /// A picker with swatches of the given colours, named after their places — "0", "1" — and the
    /// cursor on the first.
    fn coloured(colours: &[Rgb], art: &str) -> Picker {
        let mut palette = Palette::new();
        for (at, colour) in colours.iter().enumerate() {
            palette.push(at.to_string(), *colour).expect("distinct colours");
        }
        let mut picker = Picker::new(Canvas::from_text(art).expect("valid art"))
            .with_palette(palette)
            .with_rng(Rng::from_seed(7));
        picker.set_focus(Focus::Swatch { at: 0 });
        picker
    }

    /// What the dial shows, channel by channel.
    fn shown(picker: &Picker) -> [u16; 3] {
        let dial = picker.dial().expect("dialling");
        Channel::ALL.map(|channel| dial.shown(channel))
    }

    /// `[+]` opens the dial on a colour nothing holds, and adds nothing yet: Esc there leaves the
    /// palette, the history and the cursor as they were.
    #[test]
    fn the_button_opens_the_dial_on_a_free_colour_and_esc_adds_nothing() {
        let mut picker = picker(2, "ab");
        picker.set_focus(Focus::Add);
        assert_eq!(apply(&mut picker, ENTER), Action::Redraw);
        let start = picker.dial().expect("the dial is open").start();
        assert!(picker.palette().holder_of(start).is_none(), "{start} is free");
        assert_eq!(apply(&mut picker, ESC), Action::Redraw);
        assert_eq!(picker.dial(), None);
        assert_eq!(picker.palette().len(), 2, "nothing added");
        assert!(!picker.is_dirty(), "and nothing recorded");
        assert_eq!(picker.focus(), Focus::Add);
    }

    /// The colour a new swatch gets is the colour on the dial when Enter kept it — turned there
    /// channel by channel.
    #[test]
    fn a_new_swatch_is_the_colour_dialled() {
        let mut picker = picker(0, "ab");
        press(&mut picker, &[ENTER, UP, UP, RIGHT, DOWN, RIGHT, PAGE_DOWN]);
        let dial = *picker.dial().expect("dialling");
        assert!(dial.is_changed());
        press(&mut picker, &[ENTER]);
        assert_eq!(picker.palette().at(0).map(Swatch::color), Some(dial.rgb().into()));
        assert_eq!(picker.editing(), Some("colour 1"), "and it is being named");
    }

    /// F2 starts the dial from the swatch's own colour. Esc gives it up and changes nothing: not
    /// the swatch, not the drawing, not the history.
    #[test]
    fn f2_starts_from_the_swatchs_colour_and_esc_changes_nothing() {
        let mut picker = coloured(&[RED, BLUE], "ab");
        picker.set_focus(Focus::Swatch { at: 1 });
        press(&mut picker, &[F2]);
        assert_eq!(picker.dial().map(Dial::start), Some(BLUE));
        press(&mut picker, &[UP, UP, RIGHT, DOWN]);
        assert_eq!(apply(&mut picker, ESC), Action::Redraw);
        assert_eq!(picker.palette().at(1).map(Swatch::color), Some(BLUE.into()));
        assert!(!picker.is_dirty());
        assert_eq!(picker.editing(), None, "no naming either");
    }

    /// Kept unchanged — straight away, or turned and turned back — F2 records nothing and dirties
    /// nothing, even for a colour the trip through hue, saturation and brightness would change.
    /// It goes straight on to naming.
    #[test]
    fn f2_kept_unchanged_records_nothing() {
        let awkward = Rgb::new(200, 100, 7);
        assert_ne!(Rgb::from_hsb(awkward.to_hsb()), awkward, "a colour the trip changes");
        // Saturation is at 97%: down ten and back up, since up would stop at 100.
        for keys in [&[F2, ENTER][..], &[F2, UP, DOWN, RIGHT, PAGE_DOWN, PAGE_UP, ENTER]] {
            let mut picker = coloured(&[awkward], "ab");
            press(&mut picker, keys);
            assert_eq!(picker.palette().at(0).map(Swatch::color), Some(awkward.into()));
            assert!(!picker.is_dirty(), "{keys:?}");
            assert_eq!(picker.editing(), Some("0"), "naming, as F2 always goes on to");
        }
    }

    /// Kept changed, F2 recolours the swatch and every cell painted with it, as one edit that
    /// undoes whole.
    #[test]
    fn f2_kept_changed_recolours_the_drawing_and_undoes_whole() {
        let mut picker = ready_to_paint("abc");
        press(&mut picker, &[SPACE, RIGHT, SPACE]);
        let before = picker.palette().at(0).unwrap().color();
        picker.set_focus(Focus::Swatch { at: 0 });
        press(&mut picker, &[F2, PAGE_UP, ENTER, ENTER]); // ten degrees round, kept; the name too
        let after = picker.palette().at(0).unwrap().color();
        assert_ne!(after, before);
        assert_eq!(inks(&picker, 0), [Some(after), Some(after), None], "the drawing moved with it");
        assert!(picker.undo());
        assert_eq!(inks(&picker, 0), [Some(before), Some(before), None]);
        assert_eq!(picker.palette().at(0).unwrap().color(), before);
    }

    /// A slot swatch starts from the colour the slot usually is, and stays the slot unless that
    /// colour is changed — with no notice, since nothing about it changed.
    #[test]
    fn a_slot_swatch_stays_a_slot_unless_its_colour_changes() {
        let mut picker = slotted(196);
        picker.set_focus(Focus::Swatch { at: 0 });
        press(&mut picker, &[F2]);
        assert_eq!(picker.dial().map(Dial::start), Some(Ink::Slot(196).approximate()));
        press(&mut picker, &[UP, DOWN, ENTER]);
        assert_eq!(picker.palette().get("sea").unwrap().color(), Ink::Slot(196));
        assert_eq!(picker.notice(), None);
        assert!(!picker.is_dirty());
    }

    /// A colour another swatch holds can be dialled past, but not kept: the status says whose it
    /// is before Enter is pressed, Enter refuses with a notice, and the dial stays open to turn on.
    #[test]
    fn a_colour_another_swatch_holds_is_refused_and_the_dial_stays_open() {
        let mut one_up = Dial::new(RED);
        one_up.turn(1);
        let mut picker = coloured(&[RED, one_up.rgb()], "ab");
        press(&mut picker, &[F2, UP]);
        let status = console::strip_ansi_codes(&status_of(&picker)).into_owned();
        assert!(status.contains("taken by \"1\""), "{status}");
        assert_eq!(apply(&mut picker, ENTER), Action::Redraw);
        assert!(
            picker.notice().is_some_and(|n| n.starts_with("not kept")),
            "{:?}",
            picker.notice()
        );
        assert!(picker.dial().is_some(), "still dialling");
        assert_eq!(picker.palette().at(0).map(Swatch::color), Some(RED.into()), "unchanged");
        press(&mut picker, &[UP, ENTER]);
        assert_eq!(picker.dial(), None, "a free colour is kept");
        assert_ne!(picker.palette().at(0).map(Swatch::color), Some(RED.into()));
    }

    /// The same for `[+]`: a colour already in the palette cannot be added twice.
    #[test]
    fn a_new_colour_someone_holds_is_refused() {
        let mut picker = picker(1, "ab");
        picker.set_focus(Focus::Add);
        press(&mut picker, &[ENTER]);
        let mut one_up = *picker.dial().expect("dialling");
        one_up.turn(1);
        picker.palette.push("there first", one_up.rgb()).expect("free until now");
        press(&mut picker, &[UP, ENTER]);
        assert!(
            picker.notice().is_some_and(|n| n.contains("there first")),
            "{:?}",
            picker.notice()
        );
        assert_eq!(picker.palette().len(), 2, "nothing added");
        assert!(picker.dial().is_some());
    }

    /// The colour being edited never blocks itself: turned away and back to where it started,
    /// it is not "taken" by its own swatch, and Enter keeps it.
    #[test]
    fn a_swatch_never_blocks_its_own_colour() {
        let mut picker = coloured(&[RED, BLUE], "ab");
        press(&mut picker, &[F2, UP, DOWN]);
        let status = console::strip_ansi_codes(&status_of(&picker)).into_owned();
        assert!(!status.contains("taken"), "{status}");
        press(&mut picker, &[ENTER]);
        assert_eq!(picker.notice(), None);
        assert_eq!(picker.dial(), None);
    }

    /// Left and Right choose the channel, stopping at either end; Up and Down turn it a step,
    /// Page Up and Page Down ten.
    #[test]
    fn left_and_right_choose_and_up_and_down_turn() {
        let mut picker = coloured(&[Rgb::new(0, 128, 255)], "ab");
        press(&mut picker, &[F2]);
        let [h, s, b] = shown(&picker);
        press(&mut picker, &[UP]);
        assert_eq!(shown(&picker), [h + 1, s, b], "hue first");
        press(&mut picker, &[RIGHT, DOWN]);
        assert_eq!(shown(&picker), [h + 1, s - 1, b]);
        press(&mut picker, &[RIGHT, PAGE_DOWN]);
        assert_eq!(shown(&picker), [h + 1, s - 1, b - 10]);
        assert_eq!(apply(&mut picker, RIGHT), Action::Ignored, "nothing right of brightness");
        press(&mut picker, &[LEFT, LEFT, PAGE_UP]);
        assert_eq!(shown(&picker), [h + 11, s - 1, b - 10], "back to hue, and ten round");
        assert_eq!(apply(&mut picker, LEFT), Action::Ignored, "nothing left of hue");
    }

    /// Holding an arrow keeps turning — a repeat is a turn, where the pen's rules would ignore it —
    /// while a held Enter keeps the colour once, not once per repeat.
    #[test]
    fn holding_an_arrow_keeps_turning_the_dial() {
        let mut picker = coloured(&[RED], "ab");
        press(&mut picker, &[F2]);
        for _ in 0..5 {
            assert_eq!(apply(&mut picker, repeat(KeyCode::Up)), Action::Redraw);
        }
        assert_eq!(apply(&mut picker, repeat(KeyCode::PageUp)), Action::Redraw);
        assert_eq!(shown(&picker)[0], 15);
        assert_eq!(apply(&mut picker, repeat(KeyCode::Enter)), Action::Ignored);
        assert!(picker.dial().is_some(), "a repeat did not keep it");
    }

    /// While the dial is open nothing but its own keys acts: no stray key paints, undoes, saves,
    /// closes, or splits the view with a colour half chosen.
    #[test]
    fn the_dial_swallows_every_key_that_is_not_its_own() {
        let mut picker = ready_to_paint("ab");
        press(&mut picker, &[SPACE]);
        picker.set_focus(Focus::Swatch { at: 0 });
        press(&mut picker, &[F2, UP]);
        let (dial, dirty) = (*picker.dial().unwrap(), picker.is_dirty());
        let strays = [SAVE, UNDO, REDO, CLOSE, BACKSPACE, DELETE, TOGGLE, F2, F6];
        for key in strays.into_iter().chain([ch('i'), ch(']'), ch('c'), ch('x')]) {
            assert_eq!(apply(&mut picker, key), Action::Ignored, "{key:?}");
        }
        assert_eq!(picker.dial(), Some(&dial), "the dial did not move");
        assert_eq!((picker.is_dirty(), picker.split(), picker.pen_size()), (dirty, None, 1));
    }

    /// Digits type straight into the value the arrows turn, taking effect at once; moving to
    /// another value ends the number, and the next digits start a new one there.
    #[test]
    fn digits_type_straight_into_the_value_the_arrows_turn() {
        let mut picker = coloured(&[RED], "ab");
        press(&mut picker, &[F2, ch('1'), ch('2'), ch('0')]);
        assert_eq!(shown(&picker)[0], 120);
        press(&mut picker, &[RIGHT, ch('5'), ch('0')]);
        assert_eq!(shown(&picker)[..2], [120, 50]);
        let dialled = picker.dial().unwrap().rgb();
        press(&mut picker, &[ENTER]);
        assert_eq!(picker.palette().at(0).map(Swatch::color), Some(dialled.into()));
    }

    /// A colour typed in hex is kept exactly — from the bytes a terminal sends, `#` included, as
    /// kitty spells it: the key `3`, with Shift, typing `#`.
    #[test]
    fn a_hex_colour_typed_is_kept_exactly() {
        let mut picker = coloured(&[RED], "ab");
        press(&mut picker, &[F2]);
        type_bytes(&mut picker, b"\x1b[51;2;35u4b0082");
        assert_eq!(picker.dial().map(Dial::rgb), Some(Rgb::new(0x4b, 0x00, 0x82)));
        press(&mut picker, &[ENTER]);
        assert_eq!(picker.palette().at(0).map(Swatch::color), Some(Rgb::new(0x4b, 0, 0x82).into()));
    }

    /// A hex colour half typed is no colour yet, and holds every key until it is finished or
    /// given up — saying why — while Esc gives up only it, and a second Esc the dial.
    #[test]
    fn a_half_typed_hex_holds_every_key_until_finished_or_given_up() {
        let mut picker = coloured(&[RED], "ab");
        press(&mut picker, &[F2, ch('#'), ch('1'), ch('e'), ch('9'), ch('f')]);
        for key in [ENTER, RIGHT, UP] {
            assert_eq!(apply(&mut picker, key), Action::Redraw, "{key:?}");
            assert!(picker.notice().is_some_and(|n| n.contains("#1e9f is not a colour yet")));
            assert_eq!(picker.dial().and_then(Dial::typed_hex), Some("1e9f"), "still typing");
        }
        assert_eq!(shown(&picker), [0, 75, 89], "nothing moved");
        assert_eq!(apply(&mut picker, ESC), Action::Redraw);
        assert_eq!(picker.dial().map(|d| d.typed_hex()), Some(None), "the hex given up");
        assert_eq!(apply(&mut picker, ESC), Action::Redraw);
        assert_eq!(picker.dial(), None, "and then the dial");
    }

    /// Tab and Shift+Tab move between the values as they do in a form, stopping at the ends as
    /// Left and Right do.
    #[test]
    fn tab_and_shift_tab_move_between_values() {
        let mut picker = coloured(&[RED], "ab");
        press(&mut picker, &[F2, TAB]);
        assert_eq!(picker.dial().map(Dial::channel), Some(Channel::Saturation));
        press(&mut picker, &[TAB, TAB]);
        assert_eq!(picker.dial().map(Dial::channel), Some(Channel::Brightness));
        press(&mut picker, &[BACKTAB]);
        assert_eq!(picker.dial().map(Dial::channel), Some(Channel::Saturation));
    }

    /// While typing, the frame shows it: the number being typed underlined, and a hex colour in
    /// the place of the colour's name with a caret where its next digit goes — which is where the
    /// `^` and `v` point.
    #[test]
    fn what_is_being_typed_shows_where_it_goes() {
        with_colour();
        let mut picker = coloured(&[RED], "ab");
        press(&mut picker, &[F2, ch('7')]);
        let line = line_showing(&picker, BodyLine::Dial(DialLine::Values));
        let underlined = stderr_style().underlined().apply_to(dial_value(Channel::Hue, 7));
        assert!(line.contains(&underlined.to_string()), "{line:?}");
        press(&mut picker, &[ch('#'), ch('a'), ch('b')]);
        let line = line_showing(&picker, BodyLine::Dial(DialLine::Values));
        let caret = stderr_style().reverse().apply_to(' ').to_string();
        assert!(line.contains(&format!("#ab{caret}")), "{line:?}");
        assert!(!line.contains(&underlined.to_string()), "the number ended at the #");
        let above =
            console::strip_ansi_codes(&line_showing(&picker, BodyLine::Dial(DialLine::Above)))
                .into_owned();
        let text = console::strip_ansi_codes(&line).into_owned();
        let caret_at = text.find("#ab").expect("the hex") + "#ab".len();
        assert_eq!(above.find('^'), Some(caret_at), "{above:?} over {text:?}");
    }

    /// Space keeps the colour as Enter does — the two mean the same everywhere — and a held Space
    /// keeps it once, not once per repeat.
    #[test]
    fn space_keeps_the_colour_as_enter_does() {
        let mut picker = coloured(&[RED], "ab");
        press(&mut picker, &[F2, UP]);
        let dialled = picker.dial().unwrap().rgb();
        assert_eq!(apply(&mut picker, repeat(KeyCode::Char(' '))), Action::Ignored, "a repeat");
        assert!(picker.dial().is_some());
        assert_eq!(apply(&mut picker, SPACE), Action::Redraw);
        assert_eq!(picker.dial(), None);
        assert_eq!(picker.palette().at(0).map(Swatch::color), Some(dialled.into()));
        assert_eq!(picker.editing(), Some("0"), "and on to the name, where Space types");
        press(&mut picker, &[SPACE]);
        assert_eq!(picker.editing(), Some("0 "));
    }

    /// Ctrl+C gives up the dial along with everything else, and interrupts.
    #[test]
    fn ctrl_c_gives_up_the_dial_and_interrupts() {
        let mut picker = coloured(&[RED], "ab");
        press(&mut picker, &[F2, UP]);
        assert_eq!(apply(&mut picker, INTERRUPT), Action::Interrupt);
        assert_eq!(picker.dial(), None);
        assert_eq!(picker.palette().at(0).map(Swatch::color), Some(RED.into()));
    }

    /// The hints while dialling are the dial's own keys — how to leave it among them — and the
    /// only other is the interrupt.
    #[test]
    fn while_dialling_the_hints_are_the_dials() {
        let mut picker = coloured(&[RED], "ab");
        press(&mut picker, &[F2]);
        let [file, draw, view] = [FooterLine::File, FooterLine::Colour, FooterLine::View]
            .map(|which| footer_line(&render(&picker, 300, 60), 60, which))
            .map(|line| console::strip_ansi_codes(&line).into_owned());
        assert_eq!(file.trim_start_matches("Program:").trim(), "^C quit");
        let offered =
            ["←→ H/S/B", "↑↓ turn", "space/enter keep", "esc cancel", "0-9 type", "# hex"];
        for offered in offered.into_iter().chain(["pgup/pgdn by 10"]) {
            assert!(draw.contains(offered), "{offered:?} in {draw:?}");
        }
        assert_eq!(view.trim_start_matches("Display:").trim(), "", "nothing on screen acts");
        picker.cancel_colour();
        picker.set_focus(Focus::Add);
        press(&mut picker, &[ENTER]);
        assert!(hints(&picker).contains("space/enter add"));
    }

    /// The dial's three lines stand where F2's swatch was, and before `[+]` for a new one — and
    /// everything after them moves down to make room, through the one map of the body.
    #[test]
    fn the_dial_stands_where_its_swatch_is() {
        let dial = DialLine::ALL.map(BodyLine::Dial);
        let (swatch, add) = (BodyLine::Swatch, BodyLine::Add);
        let body = |picker: &Picker, lines| -> Vec<BodyLine> {
            (0..lines).map(|at| picker.body_line(at)).collect()
        };
        let mut picker = coloured(&[RED, BLUE, Rgb::new(0, 200, 0)], "ab");
        picker.set_focus(Focus::Swatch { at: 1 });
        press(&mut picker, &[F2]);
        let expected: Vec<BodyLine> =
            [swatch(0)].into_iter().chain(dial).chain([swatch(2), add]).collect();
        assert_eq!(body(&picker, expected.len()), expected, "where the second swatch was");
        picker.cancel_colour();
        picker.set_focus(Focus::Add);
        press(&mut picker, &[ENTER]);
        let expected: Vec<BodyLine> =
            [swatch(0), swatch(1), swatch(2)].into_iter().chain(dial).chain([add]).collect();
        assert_eq!(body(&picker, expected.len()), expected, "before the button");
        assert_eq!(picker.body_line(picker.first_art_line()), BodyLine::Art(0));
    }

    /// Exactly one line carries the cursor's mark while dialling — the dial's middle one — for a
    /// new swatch as for F2's.
    #[test]
    fn the_dials_middle_line_is_the_cursors() {
        let mut picker = picker(2, "ab\ncd");
        for keys in [&[F2][..], &[DOWN, DOWN, ENTER]] {
            picker.cancel_colour();
            picker.set_focus(Focus::Swatch { at: 0 });
            press(&mut picker, keys);
            let values = body_index(&picker, BodyLine::Dial(DialLine::Values));
            assert_eq!(marked(&render(&picker, 200, 60)), [values], "{keys:?}");
        }
    }

    /// The `^` and `v` stand over and under the middle digit of the value the arrows turn.
    #[test]
    fn the_pointers_stand_over_the_value_the_arrows_turn() {
        let mut picker = coloured(&[Rgb::new(30, 144, 255)], "ab");
        press(&mut picker, &[F2]);
        for channel in Channel::ALL {
            let line = |part| {
                let text = line_showing(&picker, BodyLine::Dial(part));
                console::strip_ansi_codes(&text).into_owned()
            };
            let (above, values, below) =
                (line(DialLine::Above), line(DialLine::Values), line(DialLine::Below));
            let field =
                values.find(&format!("{}:", channel.letter())).expect("the channel's field");
            let middle = field + DIAL_VALUE_WIDTH - 2;
            assert_eq!(above.find('^'), Some(middle), "{channel:?}: {above:?} over {values:?}");
            assert_eq!(below.find('v'), Some(middle), "{channel:?}: {below:?}");
            press(&mut picker, &[RIGHT]);
        }
    }

    /// Under the dial, in blue, a tip: the most the value the arrows turn takes, for when it is
    /// typed — following the arrows from one channel to the next — and that a hex colour will do
    /// instead; while one is being typed, what it takes.
    #[test]
    fn the_tip_under_the_dial_names_what_can_be_typed() {
        with_colour();
        let mut picker = coloured(&[RED], "ab");
        press(&mut picker, &[F2]);
        let tip = |picker: &Picker| line_showing(picker, BodyLine::Dial(DialLine::Tip));
        let blue = stderr_style().fg(console::Color::Blue).apply_to("x").to_string();
        let blue = &blue[..blue.find('x').expect("the text")];
        assert!(tip(&picker).contains(blue), "in blue: {:?}", tip(&picker));
        for (key, expected) in [
            (None, "Max H if/when typing: 359. Or: #rrggbb"),
            (Some(RIGHT), "Max S if/when typing: 100. Or: #rrggbb"),
            (Some(RIGHT), "Max B if/when typing: 100. Or: #rrggbb"),
            (Some(ch('#')), "#rrggbb, or #rgb; esc gives it up"),
        ] {
            press(&mut picker, &key.into_iter().collect::<Vec<_>>());
            // The strips follow it on the same line, so it is found where it starts.
            let text = console::strip_ansi_codes(&tip(&picker)).into_owned();
            assert_eq!(text.find(expected), Some(DIAL_VALUES_AT), "under the values: {text:?}");
        }
    }

    /// The tip starts where the values do and ends before the strips, for every channel and
    /// while a hex colour is typed — so the strips never stand in the middle of it.
    #[test]
    fn a_dial_tip_ends_before_the_strips() {
        with_colour();
        let mut picker = coloured(&[RED], "ab");
        press(&mut picker, &[F2]);
        let mut seen = 0;
        for keys in [&[][..], &[TAB], &[TAB], &[ch('#')]] {
            press(&mut picker, keys);
            let tip = dial_tip(picker.dial().expect("dialling"));
            for wide in [false, true] {
                let width = text_width(&tip, wide);
                assert!(width <= DIAL_TIP_WIDEST.len(), "{tip:?} is wider than the widest, {wide}");
                assert!(DIAL_VALUES_AT + width < STRIPS_AT, "{tip:?}, wide {wide}");
            }
            // And in a frame the strips are drawn in, it is there whole.
            let lines = render(&picker, 120, 40);
            assert!(strip_lines(&lines, false).len() > 1, "the strips are drawn");
            let whole = lines.iter().any(|line| console::strip_ansi_codes(line).contains(&tip));
            assert!(whole, "{tip:?} in {lines:?}");
            seen += 1;
        }
        assert_eq!(seen, Channel::ALL.len() + 1, "every channel, and a hex colour");
    }

    /// The dial's middle line: the colour dialled, its three values, and its name as the file will
    /// write it — a slot's own name while the dial has not changed it, its hex once it has.
    #[test]
    fn the_dials_line_shows_the_colour_its_values_and_its_name() {
        with_colour();
        let mut picker = coloured(&[Rgb::new(255, 0, 0)], "ab");
        press(&mut picker, &[F2, UP]);
        let line = line_showing(&picker, BodyLine::Dial(DialLine::Values));
        let colour = picker.dial().unwrap().rgb();
        let Rgb { r, g, b } = colour;
        assert!(line.contains(&format!("\x1b[48;2;{r};{g};{b}m")), "the dialled colour: {line:?}");
        let text = console::strip_ansi_codes(&line).into_owned();
        assert!(text.contains(&format!("H:   1 ; S: 100 ; B: 100  {colour}")), "{text:?}");

        let mut slot = slotted(196);
        slot.set_focus(Focus::Swatch { at: 0 });
        press(&mut slot, &[F2]);
        let line = line_showing(&slot, BodyLine::Dial(DialLine::Values));
        assert!(line.contains("\x1b[48;5;196m"), "drawn as the slot until it changes: {line:?}");
        assert!(console::strip_ansi_codes(&line).contains("B: 100  slot 196"), "{line:?}");
    }

    /// A value that turning would not change the colour for is dim: a grey's hue, black's
    /// saturation too.
    #[test]
    fn a_value_with_no_effect_is_dim() {
        with_colour();
        let dim = |text: &str| stderr_style().dim().apply_to(text).to_string();
        let mut grey = coloured(&[Rgb::new(128, 128, 128)], "ab");
        press(&mut grey, &[F2]);
        let line = line_showing(&grey, BodyLine::Dial(DialLine::Values));
        assert!(line.contains(&dim("H:   0")), "{line:?}");
        assert!(!line.contains(&dim("S:   0")), "saturation still does something: {line:?}");
    }

    /// On a terminal too short for the palette, opening the dial on a swatch at the edge of the
    /// window brings all three of its lines into view — not just the middle one.
    #[test]
    fn the_view_keeps_all_three_dial_lines_in_sight() {
        let mut picker = picker(12, "ab");
        let height = 12;
        for at in [0, 11] {
            picker.cancel_colour();
            picker.set_focus(Focus::Swatch { at });
            picker.set_view(picker.scrolled(80, height));
            press(&mut picker, &[F2]);
            let view = picker.scrolled(80, height);
            let rows = view.top..view.top + body_rows(height, false);
            for part in DialLine::ALL {
                let line = body_index(&picker, BodyLine::Dial(part));
                assert!(rows.contains(&line), "swatch {at}: {part:?} at {line}, window {rows:?}");
            }
        }
    }

    // ---- the strips beside the dial ----------------------------------------------------------

    /// The escape that starts the strips' black.
    fn behind_the_strips() -> String {
        let Rgb { r, g, b } = BEHIND_THE_STRIPS;
        format!("\x1b[48;2;{r};{g};{b}m")
    }

    /// The lines of a frame that carry the strips: those whose first style at the strips' column
    /// is the strips' black.
    fn strip_lines(lines: &[String], wide: bool) -> Vec<usize> {
        let black = behind_the_strips();
        (0..lines.len())
            .filter(|at| {
                let rest = tail(&lines[*at], STRIPS_AT, wide);
                let opening = segments(&rest).take_while(Result::is_err).any(|e| e == Err(&black));
                opening
            })
            .collect()
    }

    /// Beside the dial stand three strips on black — a line of their letters, then a cell a line —
    /// centred on its middle line where there is room, each with its marker beside the cell
    /// nearest its value.
    #[test]
    fn the_strips_stand_beside_the_dial_on_black() {
        with_colour();
        let mut picker = picker(8, "ab\ncd\nef\ngh\nij");
        picker.set_focus(Focus::Swatch { at: 6 });
        press(&mut picker, &[F2, RIGHT]);
        let lines = render(&picker, 100, 40);
        let found = strip_lines(&lines, false);
        assert_eq!(found.len(), 1 + STRIP_CELLS, "a line of letters, then the cells: {found:?}");
        let values = body_index(&picker, BodyLine::Dial(DialLine::Values));
        assert!(found.windows(2).all(|pair| pair[1] == pair[0] + 1), "one block: {found:?}");
        assert_eq!(found[found.len() / 2], values, "centred on the dial's line: {found:?}");

        let letters =
            console::strip_ansi_codes(&tail(&lines[found[0]], STRIPS_AT, false)).into_owned();
        let expected: String = "HSB".chars().map(|c| format!("{c}   ")).collect();
        assert_eq!(letters, format!(" {expected}"), "a column of black, then each letter");
        let dial = picker.dial().unwrap();
        for (at, channel) in Channel::ALL.into_iter().enumerate() {
            let column = STRIPS_AT + 1 + at * (STRIP_WIDTH + 2) + STRIP_WIDTH;
            let beside: Vec<usize> = found[1..]
                .iter()
                .enumerate()
                .filter(|(_, line)| {
                    let text = console::strip_ansi_codes(&lines[**line]).into_owned();
                    text.chars().nth(column) == Some(STRIP_MARKER)
                })
                .map(|(cell, _)| cell)
                .collect();
            assert_eq!(beside, [dial.marker(channel, STRIP_CELLS)], "{channel:?}'s marker");
        }
    }

    /// Each cell of a strip is drawn in the colour the dial gives for it, top to bottom.
    #[test]
    fn a_strips_cells_are_the_colours_the_dial_gives() {
        with_colour();
        let mut picker = picker(8, "ab");
        picker.set_focus(Focus::Swatch { at: 5 });
        press(&mut picker, &[F2]);
        let lines = render(&picker, 100, 40);
        let found = strip_lines(&lines, false);
        let dial = picker.dial().unwrap();
        for channel in Channel::ALL {
            for (cell, colour) in dial.strip(channel, STRIP_CELLS).into_iter().enumerate() {
                let Rgb { r, g, b } = colour;
                let line = tail(&lines[found[1 + cell]], STRIPS_AT, false);
                assert!(line.contains(&format!("\x1b[48;2;{r};{g};{b}m  ")), "{channel:?} {cell}");
            }
        }
    }

    /// What the strips stand over is hidden only where they stand: a label running under them is
    /// the same label either side of them.
    #[test]
    fn what_runs_under_the_strips_carries_on_after_them() {
        with_colour();
        let mut picker = coloured(&[RED, BLUE], "ab");
        picker.palette.rename("1", format!("under {}", "x".repeat(60))).expect("a label");
        let plain = |picker: &Picker| {
            console::strip_ansi_codes(&line_showing(picker, BodyLine::Swatch(1))).into_owned()
        };
        let before = plain(&picker);
        press(&mut picker, &[F2]);
        let laid = plain(&picker);
        let after = STRIPS_AT + STRIPS_WIDTH;
        assert!(before.len() > after, "the label runs past the strips: {before:?}");
        assert_eq!(laid[..STRIPS_AT], before[..STRIPS_AT]);
        assert_eq!(laid[after..], before[after..]);
        assert_ne!(laid[STRIPS_AT..after], before[STRIPS_AT..after], "the strips stand between");
    }

    /// Shorter terminals get shorter strips, down to three cells; below that, and on a terminal
    /// too narrow for all of them, there are none — and nothing is cut for them.
    #[test]
    fn the_strips_shrink_to_fit_and_are_left_off_where_they_cannot() {
        with_colour();
        let mut picker = picker(1, "ab");
        press(&mut picker, &[F2]);
        for height in 4..40 {
            let lines = render(&picker, 100, height);
            // Only the body's lines can carry them, and a small drawing has few.
            let shown = body_rows(height, false).min(picker.body_len());
            let cells = STRIP_CELLS.min(shown.saturating_sub(1));
            let found = strip_lines(&lines, false).len();
            match cells >= MIN_STRIP_CELLS {
                true => assert_eq!(found, cells + 1, "at height {height}"),
                false => assert_eq!(found, 0, "at height {height}"),
            }
        }
        let narrow = render(&picker, STRIPS_AT + STRIPS_WIDTH - 1, 40);
        assert!(narrow.iter().all(|line| !line.contains(&behind_the_strips())), "{narrow:?}");
    }

    /// While F2's dial is turned, the drawing is shown as keeping the colour would leave it — and
    /// only shown: the canvas holds its colours until Enter keeps the change, and Esc takes the
    /// preview away.
    #[test]
    fn f2_previews_the_recolour_on_the_drawing_without_touching_it() {
        with_colour();
        let mut picker = ready_to_paint("abc");
        press(&mut picker, &[SPACE]);
        let own = rgb(picker.palette().at(0).unwrap().color());
        picker.set_focus(Focus::Swatch { at: 0 });
        press(&mut picker, &[F2, PAGE_UP]);
        let dialled = picker.dial().unwrap().rgb();
        let art = line_showing(&picker, BodyLine::Art(0));
        let fg = |Rgb { r, g, b }: Rgb| format!("\x1b[38;2;{r};{g};{b}m");
        assert!(art.contains(&fg(dialled)), "drawn in the dialled colour: {art:?}");
        assert!(!art.contains(&fg(own)), "{art:?}");
        assert_eq!(inks(&picker, 0)[0], Some(own.into()), "the canvas is untouched");
        press(&mut picker, &[ESC]);
        assert!(line_showing(&picker, BodyLine::Art(0)).contains(&fg(own)), "and back");
    }

    // ---- laying one line over another ----------------------------------------------------------

    /// A line shorter than where the patch goes is padded out to it.
    #[test]
    fn an_overlay_pads_a_short_line_out_to_where_it_goes() {
        assert_eq!(overlay("ab", 5, "XY", false), "ab   XY");
        assert_eq!(overlay("", 0, "XY", false), "XY");
        assert_eq!(overlay("abcdef", 2, "XY", false), "abXYef", "replaces what it covers");
    }

    /// A wide glyph either edge of the patch cuts through gives way to a space, so the patch
    /// starts where it was asked to and nothing after it moves.
    #[test]
    fn an_overlay_gives_a_wide_glyph_it_cuts_through_a_space() {
        let line = "\u{6f22}\u{5b57}\u{6f22}\u{5b57}"; // four glyphs, two columns each
        assert_eq!(overlay(line, 3, "X", false), "\u{6f22} X\u{6f22}\u{5b57}", "the left edge");
        assert_eq!(overlay(line, 2, "X", false), "\u{6f22}X \u{6f22}\u{5b57}", "the right edge");
        for at in 0..10 {
            let laid = overlay(line, at, "X", false);
            assert_eq!(text_width(&laid, false), text_width(line, false).max(at + 1), "at {at}");
        }
        // Ambiguous-width glyphs are wide only on a terminal that draws them so.
        assert_eq!(overlay("\u{b7}\u{b7}", 1, "X", true), " X\u{b7}");
        assert_eq!(overlay("\u{b7}\u{b7}", 1, "X", false), "\u{b7}X");
    }

    /// What follows the patch opens in the style that was in force there, and nothing is left
    /// switched on — before the patch, or at the end.
    #[test]
    fn an_overlay_leaves_what_follows_it_in_the_style_it_was_in() {
        with_colour();
        let red = stderr_style().fg(console::Color::Red).bold();
        let styled = red.apply_to("abcdef").to_string();
        let red_on = &styled[..styled.find('a').expect("the text")];
        let laid = overlay(&format!("{styled}gh"), 2, "X", false);
        assert_eq!(console::strip_ansi_codes(&laid), "abXdefgh");
        let (before, after) = laid.split_once('X').expect("the patch");
        assert!(!leaves_a_style_open(before), "closed before the patch: {before:?}");
        assert!(after.starts_with(&format!("{red_on}def")), "red again after it: {after:?}");
        assert!(!leaves_a_style_open(&laid), "{laid:?}");
    }

    /// Over every line a real frame draws, at every column: the patch lands where it was asked
    /// to, the line keeps its width, and no style is left on.
    #[test]
    fn an_overlay_of_any_rendered_line_keeps_its_promises() {
        with_colour();
        let mut picker = ready_to_paint("ab\u{b7}cd\u{e9}");
        picker.palette.push("\u{6f22}\u{5b57} wide", Rgb::new(1, 2, 3)).expect("free");
        press(&mut picker, &[SPACE, RIGHT, SPACE]);
        for wide in [false, true] {
            picker.set_wide_ambiguous(wide);
            for line in render(&picker, 60, 30) {
                let width = text_width(&line, wide);
                for at in 0..width + 3 {
                    let laid = overlay(&line, at, "XYZ", wide);
                    let from_at = console::strip_ansi_codes(&tail(&laid, at, wide)).into_owned();
                    assert!(from_at.starts_with("XYZ"), "at {at}: {laid:?}");
                    assert_eq!(text_width(&laid, wide), width.max(at + 3), "{line:?} at {at}");
                    assert!(!leaves_a_style_open(&laid), "{laid:?}");
                }
            }
        }
    }

    // ---- memory -----------------------------------------------------------------------------

    /// The history's running totals, recounted from nothing: what [`History::bytes`] adds up
    /// without walking the history must be exactly what walking it finds.
    fn recounted(history: &History) -> (usize, usize) {
        let heap = |list: &[Edit]| list.iter().map(Edit::heap_bytes).sum::<usize>();
        (heap(&history.done), heap(&history.undone))
    }

    /// Whatever the history goes through — strokes folded, undone, redone, a rename rewriting the
    /// labels it holds, a new edit throwing the redo list away — its running totals stay exactly
    /// what a recount finds.
    #[test]
    fn the_historys_memory_is_kept_count_of_through_everything() {
        let mut picker = ready_to_paint(&["abcdef"; 6].join("\n"));
        let check = |p: &Picker, when: &str| {
            let history = &p.history;
            assert_eq!((history.done_heap, history.undone_heap), recounted(history), "{when}");
        };
        grow(&mut picker, 2);
        press(&mut picker, &[TOGGLE, RIGHT, DOWN, RIGHT, TOGGLE]);
        check(&picker, "after a stroke");
        picker.set_focus(Focus::Add);
        press(&mut picker, &[ENTER, ENTER, ch('x'), ENTER]);
        check(&picker, "after a swatch added and named: one edit of two parts");
        assert!(picker.undo() && picker.undo(), "the swatch with its name, then the stroke");
        check(&picker, "after two undos");
        assert!(picker.redo());
        check(&picker, "after a redo");
        // A swatch added and left with the name it came with: its AddSwatch is an entry of its
        // own, so renaming it later rewrites a label the history holds, and the label's length
        // changes with it. Adding it threw away what could be redone.
        picker.set_focus(Focus::Add);
        press(&mut picker, &[ENTER, ENTER, ESC]);
        check(&picker, "after a new edit threw the redo list away");
        assert_eq!(picker.history.undone_heap, 0);
        assert!(picker.begin_rename(1));
        press(&mut picker, &[ch('y'), ch('y'), ENTER]);
        assert!(picker
            .history
            .done
            .iter()
            .any(|edit| matches!(edit, Edit::AddSwatch { label, .. } if label == "colour 2yy")));
        check(&picker, "after a rename rewrote the labels the history holds");
        // F2's new colour and its name: one edit again, with a label in each part.
        picker.set_focus(Focus::Swatch { at: 0 });
        press(&mut picker, &[F2, UP, ENTER, ch('z'), ENTER]);
        check(&picker, "after a colour and a name kept as one");
        assert!(picker.undo());
        check(&picker, "after taking both back");
        // Far from the stroke, so the paint is a change and a new edit is recorded.
        picker.set_focus(Focus::Cell { x: 5, y: 5 });
        press(&mut picker, &[SPACE]);
        check(&picker, "after a new edit threw the redo list away once more");
        assert_eq!(picker.history.undone_heap, 0);
    }

    /// A long stroke grows the list of edits to hold every cell it touches; folding them into one
    /// entry gives that room back, so it is neither paid for twice nor counted as history.
    #[test]
    fn a_folded_stroke_gives_back_the_room_it_grew() {
        let mut picker = ready_to_paint(&["abcdefghij"; 10].join("\n"));
        picker.set_focus(Focus::Cell { x: 4, y: 4 });
        grow(&mut picker, 9);
        press(&mut picker, &[SPACE]);
        let done = &picker.history.done;
        assert_eq!(done.len(), 1, "one stroke");
        assert!(
            done.capacity() <= 2 * done.len() + 64,
            "and no empty room kept: {}",
            done.capacity()
        );
    }

    /// Memory is reported in two parts — the image and its history — each as allocated.
    #[test]
    fn memory_is_reported_as_the_image_and_its_history() {
        let mut picker = ready_to_paint(&["abcdefghij"; 10].join("\n"));
        let before = picker.memory();
        assert_eq!(before.image, 10 * 10 * std::mem::size_of::<Cell>(), "a cell for each");
        assert_eq!(before.total(), before.image + before.history);
        picker.set_focus(Focus::Cell { x: 4, y: 4 });
        grow(&mut picker, 9);
        press(&mut picker, &[SPACE]);
        let after = picker.memory();
        assert_eq!(after.image, before.image, "painting does not grow the image");
        assert!(after.history >= before.history + 100 * std::mem::size_of::<Edit>(), "{after:?}");
    }

    /// Past the line, a red line at the very bottom — under all three hint lines — says how much
    /// memory there is, and how much of it is the image and how much the undo history.
    #[test]
    fn past_the_memory_line_a_red_line_under_the_hints_says_where_it_goes() {
        with_colour();
        let mut picker = ready_to_paint("abc");
        assert!(!render(&picker, 200, 60).iter().any(|l| l.contains("undo history")), "not yet");
        picker.set_memory_warning_above(0);
        let lines = render(&picker, 200, 60);
        let last = lines.last().expect("a footer");
        let memory = picker.memory();
        let red = stderr_style().fg(console::Color::Red).apply_to("x").to_string();
        assert!(last.starts_with(&red[..red.find('x').unwrap()]), "red: {last:?}");
        for part in [
            format!("memory {}", readable_bytes(memory.total())),
            format!("the image {}", readable_bytes(memory.image)),
            format!("the undo history {}", readable_bytes(memory.history)),
        ] {
            assert!(last.contains(&part), "{part:?} in {last:?}");
        }
        assert!(lines[lines.len() - 2].contains("redraw"), "under the display line: {lines:?}");
    }

    /// The warning is news, not help: short of room, the hint lines give way before it does —
    /// and it gives way only to the way out and the status.
    #[test]
    fn the_memory_warning_gives_way_only_to_the_way_out_and_the_status() {
        use FooterLine::{Colour, File, Memory, Status, View};
        assert_eq!(footer(1, true), [File]);
        assert_eq!(footer(2, true), [Status, File]);
        assert_eq!(footer(3, true), [Status, File, Memory]);
        assert_eq!(footer(7, true), [Status, File, Colour, Memory]);
        assert_eq!(footer(8, true), [Status, File, Colour, View, Memory]);
        assert_eq!(footer(40, false), [Status, File, Colour, View], "and without one, as ever");
    }

    #[test]
    fn bytes_read_the_way_a_person_says_them() {
        assert_eq!(readable_bytes(512), "512 bytes");
        assert_eq!(readable_bytes(480_000), "480.0 KB");
        assert_eq!(readable_bytes(648_000_000), "648.0 MB");
        assert_eq!(readable_bytes(2_600_000_000), "2.6 GB");
    }

    // ---- F5, and the split view -------------------------------------------------------------

    #[test]
    fn f5_asks_the_caller_to_redraw_everything() {
        let mut picker = picker(1, "ab");
        assert_eq!(apply(&mut picker, F5), Action::Refresh);
        assert_eq!(type_bytes(&mut picker, b"\x1b[15~"), Action::Refresh, "from its real bytes");
    }

    #[test]
    fn f6_splits_the_art_and_puts_it_back_with_the_cursor_where_it_was() {
        let mut picker = ready_to_paint("ab\ncd");
        picker.set_focus(Focus::Cell { x: 1, y: 1 });
        assert!(!picker.is_split());
        assert_eq!(apply(&mut picker, F6), Action::Redraw);
        assert!(picker.is_split());
        assert_eq!(picker.focus(), Focus::Cell { x: 1, y: 1 }, "the same cell, in the canvas");
        assert_eq!(type_bytes(&mut picker, b"\x1b[17~"), Action::Redraw, "from its real bytes");
        assert!(!picker.is_split());
    }

    fn split(picker: &Picker) -> Picker {
        let mut s = picker.clone();
        s.split = Some(Split::Stacked);
        s
    }

    /// Palette, button, gap, then one border holding the canvas above the coloured art with a
    /// band between them. The preview carries no gutter mark, because it is not somewhere the
    /// cursor can go — the mark stays on the canvas row.
    #[test]
    fn the_split_view_is_a_block_canvas_over_a_coloured_preview() {
        with_colour();
        let mut picker = ready_to_paint("ab\ncd");
        press(&mut picker, &[SPACE, release(KeyCode::Char(' '))]); // (0,0) painted
        let picker = split(&picker);
        let lines = render(&picker, 200, 60);
        assert_eq!(lines.len(), 1 + 1 + 1 + 1 + 2 + 1 + 2 + 1 + 4, "{lines:#?}");
        let canvas: Vec<_> = (0..2).map(|y| line_showing(&picker, BodyLine::Art(y))).collect();
        let preview: Vec<_> = (0..2).map(|y| line_showing(&picker, BodyLine::Preview(y))).collect();
        assert!(canvas.iter().all(|l| !l.contains('a') && !l.contains('c')), "blocks, not glyphs");
        assert!(preview[0].contains('b') && preview[1].contains("cd"), "the art itself");
        assert_eq!(
            marked(&lines),
            [body_index(&picker, BodyLine::Art(0))],
            "one mark, on the canvas"
        );
        let between = body_index(&picker, BodyLine::Art(1)) + 1;
        assert_eq!(picker.body_line(between), BodyLine::Border, "a band between the halves");
    }

    /// Shift+F6 puts the halves side by side: one line per row, the canvas's cells then the art's,
    /// each in the border — and no scrolling needed to see both.
    #[test]
    fn shift_f6_splits_side_by_side_and_each_layout_key_toggles_its_own_layout() {
        with_colour();
        let mut picker = ready_to_paint("ab\ncd");
        let shift_f6 =
            KeyEvent { mods: crate::keys::Mods { shift: true, ..Default::default() }, ..F6 };
        assert_eq!(apply(&mut picker, shift_f6), Action::Redraw);
        assert_eq!(picker.split(), Some(Split::SideBySide));
        let row = line_showing(&picker, BodyLine::Art(1));
        let edge = border_cell();
        assert_eq!(row.matches(&edge).count(), 3, "border before, between and after: {row:?}");
        assert!(row.ends_with(&format!("cd{edge}")), "the art on the right: {row:?}");
        assert_eq!(picker.body_len(), body_index(&picker, BodyLine::Art(1)) + 2, "no second half");

        assert_eq!(apply(&mut picker, F6), Action::Redraw, "F6 from side by side: stacked");
        assert_eq!(picker.split(), Some(Split::Stacked));
        apply(&mut picker, F6);
        assert_eq!(picker.split(), None, "F6 again: joined");
        apply(&mut picker, shift_f6);
        apply(&mut picker, shift_f6);
        assert_eq!(picker.split(), None, "shift+F6 twice: out again");
        assert_eq!(type_bytes(&mut picker, b"\x1b[17;2~"), Action::Redraw, "from its real bytes");
        assert_eq!(picker.split(), Some(Split::SideBySide));
    }

    /// Side by side, each half gets its share of the width, and scrolling sideways moves both.
    #[test]
    fn side_by_side_halves_share_the_width_and_scroll_together() {
        with_colour();
        let wide: String = (0..40).map(|n| char::from(b'a' + (n % 26) as u8)).collect();
        let mut picker = picker(0, &wide);
        picker.split = Some(Split::SideBySide);
        picker.set_focus(Focus::Cell { x: 39, y: 0 });
        let lines = render(&picker, 40, 20);
        let row = &lines[body_index(&picker, BodyLine::Art(0)) - picker.scrolled(40, 20).top];
        assert!(console::measure_text_width(row) <= 40, "{row:?}");
        // 40 columns: 2 of gutter, 3 of border, and 17 for each half.
        assert_eq!(pane_columns(40, picker.split), 17);
        let plain = console::strip_ansi_codes(row);
        let tail: String = wide.chars().skip(40 - 17).collect();
        // The preview's last 17 columns, with the art cursor's `+` on the cursor's own cell.
        let shown = format!("{}+ ", &tail[..16]);
        assert!(plain.ends_with(&shown), "the preview's window: {plain:?}");
        assert!(footer_line(&lines, 20, FooterLine::Status).contains("cols 24-40 of 40"));
    }

    /// The preview shows where the cursor is — a `+` on a filled cell, the canvas's own marker —
    /// until `c` hides it, and `c` only means that while there is a preview to mark.
    #[test]
    fn c_toggles_the_cursor_on_the_art_preview() {
        with_colour();
        let mut picker = ready_to_paint("ab");
        assert_eq!(apply(&mut picker, ch('c')), Action::Ignored, "no preview, nothing to mark");
        for layout in [Split::Stacked, Split::SideBySide] {
            let mut picker = picker.clone();
            picker.split = Some(layout);
            let preview = |p: &Picker| match layout {
                Split::Stacked => line_showing(p, BodyLine::Preview(0)),
                Split::SideBySide => line_showing(p, BodyLine::Art(0)),
            };
            let marker = block_cursor(UNINKED, 1);
            let expected = if layout == Split::Stacked { 1 } else { 2 };
            assert_eq!(preview(&picker).matches(&marker).count(), expected, "{layout:?} shown");
            assert!(preview(&picker).contains(&format!("{marker}b")), "{layout:?}: on the `a`");
            assert_eq!(apply(&mut picker, ch('c')), Action::Redraw);
            assert_eq!(
                preview(&picker).matches(&marker).count(),
                expected - 1,
                "{layout:?} hidden"
            );
            assert!(hints(&picker).contains("c show art cursor"), "{}", hints(&picker));
            apply(&mut picker, ch('c'));
            assert!(hints(&picker).contains("c hide art cursor"));
        }
    }

    #[test]
    fn on_the_block_canvas_an_uninked_cell_is_black_and_an_inked_one_its_colour() {
        with_colour();
        let mut picker = ready_to_paint("abc");
        let Rgb { r, g, b } = rgb(picker.palette().at(0).unwrap().color());
        press(&mut picker, &[SPACE, release(KeyCode::Char(' '))]);
        picker.set_focus(Focus::Add); // cursor off the canvas, so no marker in the way
        let row = &line_showing(&split(&picker), BodyLine::Art(0));
        assert!(row.contains(&format!("\x1b[48;2;{r};{g};{b}m ")), "painted cell: {row:?}");
        assert!(row.contains("\x1b[48;2;0;0;0m  "), "the two unpainted ones, black: {row:?}");
    }

    /// One span per run of a colour, not one per cell — the difference between a frame that
    /// follows the drawing's colour changes and 300 KB of escapes per keystroke.
    #[test]
    fn the_block_canvas_draws_each_run_of_a_colour_once() {
        with_colour();
        let mut picker = ready_to_paint("abcdefgh");
        press(&mut picker, &[SPACE, RIGHT, RIGHT, release(KeyCode::Char(' '))]); // abc painted
        picker.set_focus(Focus::Add);
        let row = &line_showing(&split(&picker), BodyLine::Art(0));
        let spans = row.matches("\x1b[48;2;").count() - row.matches(&border_bg()).count();
        assert_eq!(spans, 2, "painted run + black run, inside the border: {row:?}");
    }

    #[test]
    fn the_block_cursor_stands_out_from_its_cell() {
        with_colour();
        let picker = ready_to_paint("ab");
        // Checked as a set, not a sequence: `console` writes the foreground before the
        // background whatever order the style was built in, and that order is its business.
        let on_black = block_cursor(Rgb::new(0, 0, 0).into(), 1);
        assert!(on_black.contains("\x1b[48;2;0;0;0m"), "on its cell's black: {on_black:?}");
        assert!(on_black.contains("\x1b[38;2;255;255;255m"), "in white: {on_black:?}");
        assert!(on_black.contains(BLOCK_CURSOR));
        let on_light = block_cursor(Rgb::new(250, 250, 200).into(), 1);
        assert!(on_light.contains("\x1b[38;2;0;0;0m"), "black on a light cell: {on_light:?}");
        let row = &line_showing(&split(&picker), BodyLine::Art(0));
        assert!(row.contains(&on_black), "and that is what the canvas row draws: {row:?}");
    }

    /// The two halves show one drawing: paint on the canvas and the preview takes the colour.
    #[test]
    fn painting_in_the_split_view_shows_in_both_halves() {
        with_colour();
        let mut picker = ready_to_paint("ab");
        picker.split = Some(Split::Stacked);
        press(&mut picker, &[SPACE, release(KeyCode::Char(' '))]);
        let Rgb { r, g, b } = rgb(picker.palette().at(0).unwrap().color());
        picker.set_focus(Focus::Add);
        let canvas = line_showing(&picker, BodyLine::Art(0));
        let preview = line_showing(&picker, BodyLine::Preview(0));
        assert!(canvas.contains(&format!("\x1b[48;2;{r};{g};{b}m")), "canvas: {canvas:?}");
        assert!(preview.contains(&format!("\x1b[38;2;{r};{g};{b}ma")), "preview: {preview:?}");
    }

    /// The split view is the tallest and most heavily styled frame the picker draws, so the three
    /// hard limits are swept over it too: never taller or wider than the terminal, and never a
    /// line ending with a colour still on.
    #[test]
    fn the_split_view_keeps_every_limit_the_single_view_keeps() {
        with_colour();
        let mut picker = ready_to_paint(&["abcdef"; 6].join("\n"));
        press(&mut picker, &[SPACE, RIGHT, DOWN, release(KeyCode::Char(' '))]);
        let picker = split(&picker);
        for width in 1..30 {
            for height in 0..25 {
                let lines = render(&picker, width, height);
                assert!(lines.len() <= height, "height {height}");
                for line in &lines {
                    assert!(console::measure_text_width(line) <= width, "{width}: {line:?}");
                    assert!(!leaves_a_style_open(line), "{width}x{height}: {line:?}");
                }
            }
        }
    }

    #[test]
    fn the_hint_says_hold_where_releases_are_reported_and_tap_where_they_are_not() {
        with_colour();
        let mut picker = ready_to_paint("ab");
        assert!(hints(&picker).contains("space paint a cell"), "{}", hints(&picker));
        picker.set_hold_keys(true);
        assert!(hints(&picker).contains("space hold to paint"), "{}", hints(&picker));
        for toggle in ["b toggle painting", "del toggle erasing"] {
            assert!(hints(&picker).contains(toggle), "the toggles are named either way");
        }
        assert!(hints(&picker).contains("F5 redraw") && hints(&picker).contains("F6 split"));
    }

    // ---- naming ---------------------------------------------------------------------------

    #[test]
    fn typing_backspace_and_enter_rename_the_swatch() {
        let mut picker = picker(0, "ab");
        press(&mut picker, &[ENTER, ENTER]);
        press(&mut picker, &vec![BACKSPACE; "colour 1".len()]);
        assert_eq!(picker.editing(), Some(""));
        assert_eq!(apply(&mut picker, BACKSPACE), Action::Ignored, "nothing left to delete");
        press(&mut picker, &[ch('s'), ch('k'), ch('y')]);
        assert_eq!(picker.editing(), Some("sky"));
        assert_eq!(apply(&mut picker, ENTER), Action::Redraw);
        assert_eq!(picker.editing(), None);
        assert_eq!(picker.palette().at(0).unwrap().label(), "sky");
        assert!(picker.palette().get("colour 1").is_none());
    }

    /// Labels may contain spaces, so Space types one rather than picking a brush.
    #[test]
    fn space_types_into_a_name_while_editing() {
        let mut picker = picker(0, "ab");
        press(&mut picker, &[ENTER, ENTER, SPACE, ch('b')]);
        assert_eq!(picker.editing(), Some("colour 1 b"));
        assert_eq!(picker.brush(), None, "and did not pick a brush");
    }

    #[test]
    fn esc_gives_up_the_rename_and_leaves_no_trace() {
        let mut picker = picker(0, "ab");
        press(&mut picker, &[ENTER, ENTER, ch('x'), ch('y')]);
        assert_eq!(apply(&mut picker, ESC), Action::Redraw);
        assert_eq!(picker.editing(), None);
        assert_eq!(picker.palette().at(0).unwrap().label(), "colour 1");
        assert_eq!(picker.history.done.len(), 1, "only the AddSwatch is recorded");
    }

    /// A name the palette will not take is reported and left on screen to be fixed, rather than
    /// thrown away.
    #[test]
    fn a_refused_name_stays_editable_with_a_notice() {
        let mut picker = picker(1, "ab"); // "colour 1" exists
        picker.set_focus(Focus::Add);
        press(&mut picker, &[ENTER, ENTER]); // "colour 3"? no — "colour 2" is the next free name
        press(&mut picker, &vec![BACKSPACE; "colour 2".len()]);
        for c in "colour 1".chars() {
            press(&mut picker, &[ch(c)]);
        }
        assert_eq!(apply(&mut picker, ENTER), Action::Redraw);
        assert!(
            picker.notice().is_some_and(|n| n.contains("not renamed")),
            "{:?}",
            picker.notice()
        );
        assert_eq!(picker.editing(), Some("colour 1"), "still editing, text kept");

        press(&mut picker, &vec![BACKSPACE; "colour 1".len()]);
        apply(&mut picker, ENTER);
        assert!(picker.notice().is_some_and(|n| n.contains("empty")), "{:?}", picker.notice());
        assert_eq!(picker.editing(), Some(""), "an empty name is refused too");

        press(&mut picker, &[ESC]);
        assert_eq!(picker.palette().at(1).unwrap().label(), "colour 2", "back to the original");
    }

    /// While a name is being typed, nothing else may act: a stray arrow must not paint and a
    /// stray Ctrl+S must not save a half-name.
    #[test]
    fn keys_that_are_not_typing_are_swallowed_while_editing() {
        let mut picker = picker(1, "ab");
        picker.set_focus(Focus::Add);
        press(&mut picker, &[ENTER, ENTER]);
        let focus = picker.focus();
        for key in [DOWN, UP, TAB, SAVE, UNDO, DELETE] {
            let named = format!("{key:?}");
            assert_eq!(apply(&mut picker, key), Action::Ignored, "{named}");
        }
        assert_eq!(picker.focus(), focus, "the cursor did not move");
        assert_eq!(picker.editing(), Some("colour 2"), "the text did not change");
    }

    #[test]
    fn renaming_the_brush_follows_it() {
        let mut picker = picker(1, "ab");
        picker.set_focus(Focus::Swatch { at: 0 });
        press(&mut picker, &[ENTER]);
        assert_eq!(picker.brush(), Some("colour 1"));
        press(&mut picker, &[F2, ENTER]); // the colour as it was, and naming
        press(&mut picker, &vec![BACKSPACE; "colour 1".len()]);
        press(&mut picker, &[ch('s'), ENTER]);
        assert_eq!(picker.brush(), Some("s"), "the brush is the same swatch under its new name");
    }

    #[test]
    fn a_rename_is_undoable_and_redoable() {
        let mut picker = picker(1, "ab");
        press(&mut picker, &[F2, ENTER]); // the colour as it was, and naming
        press(&mut picker, &[BACKSPACE, ch('x'), ENTER]); // "colour x"
        assert_eq!(picker.palette().at(0).unwrap().label(), "colour x");
        assert!(picker.undo(), "takes back the rename");
        assert_eq!(picker.palette().at(0).unwrap().label(), "colour 1");
        assert!(!picker.undo(), "and that was all there was");
        assert!(picker.redo());
        assert_eq!(picker.palette().at(0).unwrap().label(), "colour x");
    }

    /// F2's new colour and the name given straight after are one undo and one redo: it was one
    /// thing to do, and taking back the name alone would leave a colour nobody asked to keep.
    #[test]
    fn f2s_colour_and_name_are_one_undo() {
        let mut picker = ready_to_paint("ab");
        press(&mut picker, &[SPACE]);
        let before = picker.palette().at(0).unwrap().color();
        picker.set_focus(Focus::Swatch { at: 0 });
        press(&mut picker, &[F2, PAGE_UP, ENTER, ch('!'), ENTER]);
        let after = picker.palette().at(0).unwrap().color();
        assert_eq!(
            (picker.palette().at(0).unwrap().label(), inks(&picker, 0)[0]),
            ("colour 1!", Some(after))
        );
        let done = picker.history.done.len();
        assert!(picker.undo(), "one undo");
        assert_eq!(picker.palette().at(0).unwrap().label(), "colour 1", "the name back");
        assert_eq!(picker.palette().at(0).unwrap().color(), before, "and the colour");
        assert_eq!(inks(&picker, 0)[0], Some(before), "and the drawing");
        assert_eq!(picker.history.done.len(), done - 1);
        assert!(picker.redo(), "one redo");
        assert_eq!(picker.palette().at(0).unwrap().label(), "colour 1!");
        assert_eq!(inks(&picker, 0)[0], Some(after));
        assert_eq!(picker.brush(), Some("colour 1!"), "the brush followed it both ways");
    }

    /// The same for `[+]`: the swatch and the name given it straight after are one undo — taking
    /// it back takes the swatch away — and one redo.
    #[test]
    fn the_buttons_swatch_and_its_name_are_one_undo() {
        let mut picker = picker(0, "ab");
        press(&mut picker, &[ENTER, ENTER, ch('!'), ENTER]);
        assert_eq!(picker.palette().at(0).map(Swatch::label), Some("colour 1!"));
        assert!(picker.undo());
        assert!(picker.palette().is_empty(), "gone in one");
        assert!(!picker.undo(), "and nothing else was recorded");
        assert!(picker.redo());
        assert_eq!(picker.palette().at(0).map(Swatch::label), Some("colour 1!"), "back, named");
    }

    /// A name refused as it is kept — one another swatch has — leaves what came before it alone:
    /// F2's new colour stays recorded, and undoes on its own once the naming is given up.
    #[test]
    fn a_name_refused_as_it_is_kept_leaves_the_colour_undoable() {
        let mut picker = coloured(&[RED, BLUE], "ab");
        press(&mut picker, &[F2, UP, ENTER, BACKSPACE, ch('1'), ENTER]); // "0" -> "1", taken
        assert!(
            picker.notice().is_some_and(|n| n.contains("not renamed")),
            "{:?}",
            picker.notice()
        );
        assert_eq!(picker.editing(), Some("1"), "still naming");
        press(&mut picker, &[ESC]);
        assert_ne!(picker.palette().at(0).map(Swatch::color), Some(RED.into()));
        assert!(picker.undo(), "the colour is still there to take back");
        assert_eq!(picker.palette().at(0).map(Swatch::color), Some(RED.into()));
    }

    /// An edit of two parts that cannot be undone WHOLE is not undone at all: here the colour F2
    /// moved away from is taken meanwhile — by a caller changing the palette directly, which is
    /// the only way it can be — and the name, which could have gone back, stays too, as does the
    /// history. Replayed a part at a time, the name would have gone back on its own.
    #[test]
    fn an_edit_that_cannot_be_undone_whole_is_not_undone_at_all() {
        let mut picker = coloured(&[RED], "ab");
        press(&mut picker, &[F2, UP, ENTER, ch('!'), ENTER]);
        picker.palette.push("intruder", RED).expect("RED is free now");
        let (palette, done) = (picker.palette.clone(), picker.history.done.len());
        assert_eq!(apply(&mut picker, UNDO), Action::Redraw, "the refusal is shown");
        assert!(
            picker.notice().is_some_and(|n| n.contains("cannot undo")),
            "{:?}",
            picker.notice()
        );
        assert_eq!(picker.palette, palette, "not the name, not the colour");
        assert_eq!(picker.history.done.len(), done, "and still there to try again");
    }

    /// A stroke is replayed part by part in the order the parts were made — backwards to undo —
    /// however deeply strokes nest, since a replay that nested one level and flattened the next
    /// would take the parts of the inner one back in the wrong order.
    #[test]
    fn nested_strokes_are_replayed_in_order_both_ways() {
        let paint = |x| Edit::Paint { x, y: 0, from: None, to: None };
        let edit = Edit::Stroke(vec![
            paint(0),
            Edit::Stroke(vec![paint(1), Edit::Stroke(vec![paint(2)]), paint(3)]),
            paint(4),
        ]);
        for (way, expected) in [(Way::Forward, [0, 1, 2, 3, 4]), (Way::Back, [4, 3, 2, 1, 0])] {
            let mut seen = Vec::new();
            edit.for_each_part(way, &mut |part| {
                if let Edit::Paint { x, .. } = part {
                    seen.push(*x);
                }
                Ok::<(), ()>(())
            })
            .expect("nothing refuses");
            assert_eq!(seen, expected, "{way:?}");
        }
        assert!(!edit.moves_the_palette(), "paint alone asks nothing of the palette");
    }

    /// And the same going forward: a redo that would be refused half way is refused whole.
    #[test]
    fn an_edit_that_cannot_be_redone_whole_is_not_redone_at_all() {
        let mut picker = coloured(&[RED], "ab");
        press(&mut picker, &[F2, UP, ENTER, ch('!'), ENTER]);
        assert!(picker.undo());
        picker.palette.push("0!", BLUE).expect("the name is free now"); // where the rename goes
        let palette = picker.palette.clone();
        assert!(!picker.redo());
        assert!(
            picker.notice().is_some_and(|n| n.contains("cannot redo")),
            "{:?}",
            picker.notice()
        );
        assert_eq!(picker.palette, palette, "the colour did not move on its own");
    }

    /// The history holds labels, so a rename has to be carried into it — or undoing the button
    /// that made the swatch would try to remove it by a name it no longer has.
    #[test]
    fn undoing_an_added_swatch_after_renaming_it_still_finds_it() {
        let mut picker = picker(0, "ab");
        press(&mut picker, &[ENTER, ENTER, ENTER]); // added, as "colour 1"
        press(&mut picker, &[F2, ENTER]); // later: the colour as it was, and a new name
        press(&mut picker, &vec![BACKSPACE; "colour 1".len()]);
        press(&mut picker, &[ch('s'), ENTER]);
        assert_eq!(picker.palette().at(0).unwrap().label(), "s");
        assert!(picker.undo(), "the rename");
        assert!(picker.undo(), "the addition, by the name it has again");
        assert!(picker.palette().is_empty());
        assert!(picker.redo() && picker.redo());
        assert_eq!(picker.palette().at(0).unwrap().label(), "s");
    }

    // ---- undo, redo, and the file --------------------------------------------------------------

    #[test]
    fn undo_takes_back_a_paint_and_redo_puts_it_again() {
        let mut picker = ready_to_paint("ab");
        let ink = picker.palette().at(0).unwrap().color();
        press(&mut picker, &[ENTER]);

        assert_eq!(apply(&mut picker, UNDO), Action::Redraw);
        assert_eq!(inks(&picker, 0)[0], None);
        assert_eq!(apply(&mut picker, UNDO), Action::Ignored, "nothing older");
        assert_eq!(apply(&mut picker, REDO), Action::Redraw);
        assert_eq!(inks(&picker, 0)[0], Some(ink));
        assert_eq!(apply(&mut picker, REDO), Action::Ignored, "nothing newer");
    }

    /// A new edit after an undo starts a new future: what was undone can no longer be redone.
    #[test]
    fn a_new_edit_after_undo_discards_the_redo_stack() {
        let mut picker = ready_to_paint("ab");
        press(&mut picker, &[ENTER, UNDO]);
        picker.set_focus(Focus::Cell { x: 1, y: 0 });
        press(&mut picker, &[ENTER]);
        assert_eq!(apply(&mut picker, REDO), Action::Ignored);
    }

    /// Undoing the button removes the swatch it added — and if that swatch was the brush, the
    /// brush is gone with it rather than pointing at nothing.
    #[test]
    fn undoing_an_added_swatch_removes_it_and_drops_it_as_the_brush() {
        let mut picker = picker(0, "ab");
        press(&mut picker, &[ENTER, ENTER, ENTER]); // [+], keep its colour, keep the name
        press(&mut picker, &[ENTER]); // pick it
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
        let mut picker = ready_to_paint("ab");
        assert!(!picker.is_dirty(), "fresh");
        press(&mut picker, &[ENTER]);
        assert!(picker.is_dirty());

        picker.history.mark_saved();
        assert!(!picker.is_dirty(), "saved");
        press(&mut picker, &[UNDO]);
        assert!(picker.is_dirty(), "one behind the save");
        press(&mut picker, &[REDO]);
        assert!(!picker.is_dirty(), "back on it");

        press(&mut picker, &[UNDO]);
        picker.set_focus(Focus::Cell { x: 1, y: 0 });
        press(&mut picker, &[ENTER]); // same length as when saved, different content
        assert!(picker.is_dirty(), "the save point is no longer reachable");
    }

    #[test]
    fn a_picker_built_in_code_has_nowhere_to_save_or_salvage() {
        let mut picker = ready_to_paint("ab");
        press(&mut picker, &[ENTER]);
        assert!(picker.save().is_err());
        assert_eq!(picker.salvage_path(), None);
        assert_eq!(picker.salvage().expect("nothing to write is not an error"), None);
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
        press(&mut picker, &[ENTER, ENTER, ENTER]); // add a colour, keep it, keep its name
        press(&mut picker, &[ENTER]); // pick it
        picker.set_focus(Focus::Cell { x: 1, y: 1 });
        press(&mut picker, &[ENTER]); // paint d
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

    /// Ctrl+C's promise: nothing is lost. The unsaved state goes beside the file, the file itself
    /// is untouched, and a clean session leaves nothing behind.
    #[test]
    fn salvage_writes_beside_the_file_only_when_there_is_something_unsaved() {
        let dir = std::env::temp_dir().join(format!("arterminal-salvage-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("art.txt");
        std::fs::write(&path, "ab\n").expect("write");
        let mut picker = Picker::open(&path).expect("opens");
        let expected = dir.join("art.txt.arterminal.tmp");
        assert_eq!(picker.salvage_path().as_deref(), Some(expected.as_path()));

        assert_eq!(picker.salvage().expect("ok"), None, "clean: nothing to keep");
        assert!(!expected.exists());

        press(&mut picker, &[ENTER, ENTER, ENTER, ENTER]); // add: dial, keep it, keep name; pick
        picker.set_focus(Focus::Cell { x: 0, y: 0 });
        press(&mut picker, &[ENTER]);
        let ink = picker.palette().at(0).unwrap().color();
        assert_eq!(picker.salvage().expect("ok"), Some(expected.clone()), "dirty: kept");
        let kept = Picker::open(&expected).expect("the salvage is a document");
        assert_eq!(kept.canvas().cell(0, 0).unwrap().ink, Some(ink));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "ab\n", "the real file is untouched");
        assert!(picker.is_dirty(), "and the picker still knows the real file is behind");
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
        picker.canvas_mut().cell_mut(0, 0).unwrap().ink = Some(RED.into());
        picker.canvas_mut().cell_mut(1, 0).unwrap().ink = Some(BLUE.into());
        // Cell 2 stays uninked, so "rewrite the matching cells" must not mean "rewrite all".

        let moved: Ink = Rgb::new(20, 200, 40).into();
        picker.set_swatch_color("ember", moved).expect("that colour is free");
        assert_eq!(inks(&picker, 0), [Some(moved), Some(BLUE.into()), None]);
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
        picker.canvas_mut().cell_mut(0, 0).unwrap().ink = Some(RED.into());
        picker.canvas_mut().cell_mut(1, 0).unwrap().ink = Some(shade);

        let moved: Ink = Rgb::new(20, 200, 40).into();
        picker.set_swatch_color("base", moved).expect("free");

        let now_shade = picker.palette().get("shade").expect("still here").color();
        assert_ne!(now_shade, shade, "the follower moved");
        assert_eq!(inks(&picker, 0), [Some(moved), Some(now_shade)]);
    }

    /// Atomic across BOTH halves. The palette refuses the move whole; the canvas must never have
    /// been touched either, or a rejected edit would still have half-repainted the drawing.
    #[test]
    fn a_refused_move_leaves_the_drawing_exactly_as_it_was() {
        let mut palette = Palette::new();
        palette.push("ember", RED).expect("fresh");
        palette.push("sky", BLUE).expect("distinct");
        let mut picker = Picker::new(Canvas::from_text("ab").expect("valid")).with_palette(palette);
        picker.canvas_mut().cell_mut(0, 0).unwrap().ink = Some(RED.into());
        picker.canvas_mut().cell_mut(1, 0).unwrap().ink = Some(BLUE.into());
        let before = picker.canvas().clone();

        assert!(picker.set_swatch_color("ember", BLUE).is_err(), "sky is taken");
        assert_eq!(picker.canvas(), &before, "not one pixel moved");
        assert_eq!(picker.palette().get("ember").unwrap().color(), RED.into(), "nor the swatch");
    }

    /// The interaction that would otherwise put an ownerless colour back on the canvas: paint,
    /// recolour the swatch, then undo the paint. The undo must restore the swatch's colour AS IT
    /// NOW IS — the history was carried along with the move.
    #[test]
    fn undo_after_a_recolour_restores_the_colour_the_swatch_now_has() {
        let mut palette = Palette::new();
        palette.push("ember", RED).expect("fresh");
        let mut picker = Picker::new(Canvas::from_text("ab").expect("valid")).with_palette(palette);
        picker.canvas_mut().cell_mut(0, 0).unwrap().ink = Some(RED.into()); // painted before the session
        picker.set_focus(Focus::Swatch { at: 0 });
        press(&mut picker, &[ENTER]);
        picker.set_focus(Focus::Cell { x: 1, y: 0 });
        press(&mut picker, &[ENTER]); // paint b red, recorded as None -> RED

        let moved: Ink = Rgb::new(20, 200, 40).into();
        picker.set_swatch_color("ember", moved).expect("free");
        assert_eq!(inks(&picker, 0)[1], Some(moved));

        assert!(picker.undo(), "take back the paint of b");
        assert_eq!(inks(&picker, 0)[1], None);
        assert!(picker.redo(), "and put it again");
        assert_eq!(inks(&picker, 0)[1], Some(moved), "in the colour the swatch has NOW");
        assert!(
            inks(&picker, 0).iter().all(|c| c.is_none_or(|i| i == moved)),
            "no pixel holds a colour no swatch owns"
        );
    }

    /// A colour kept from F2's dial is an edit like any other: it goes, and comes back, with the
    /// history.
    #[test]
    fn a_recolour_is_undoable_and_redoable() {
        let mut picker = picker(1, "ab");
        let before = picker.palette().at(0).unwrap().color();
        press(&mut picker, &[F2, UP, ENTER]); // a degree round, kept
        press(&mut picker, &[ESC]); // keep the name as it was
        let after = picker.palette().at(0).unwrap().color();
        assert_ne!(after, before);
        assert_eq!(apply(&mut picker, UNDO), Action::Redraw);
        assert_eq!(picker.palette().at(0).unwrap().color(), before);
        assert_eq!(apply(&mut picker, REDO), Action::Redraw);
        assert_eq!(picker.palette().at(0).unwrap().color(), after);
    }

    /// Undoing a recolour can land on a colour another swatch has since taken. Uniqueness wins,
    /// the undo is refused with a notice, and nothing moves — not the swatch, not the history.
    #[test]
    fn an_undo_that_would_collide_is_refused_with_a_notice_and_changes_nothing() {
        let mut palette = Palette::new();
        palette.push("a", RED).expect("fresh");
        let mut picker = Picker::new(Canvas::from_text("ab").expect("valid")).with_palette(palette);
        press(&mut picker, &[F2, UP, ENTER]); // "a" is a degree round from RED, and RED is free
        press(&mut picker, &[ESC]);
        picker.palette.push("b", RED).expect("RED is free now");
        let colour_of_a = picker.palette().get("a").unwrap().color();
        let history_len = picker.history.done.len();

        // Nothing changed, but the refusal's notice must reach the screen: the next key would
        // otherwise clear it before any frame showed it.
        assert_eq!(apply(&mut picker, UNDO), Action::Redraw);
        assert!(
            picker.notice().is_some_and(|n| n.contains("cannot undo")),
            "{:?}",
            picker.notice()
        );
        assert_eq!(picker.palette().get("a").unwrap().color(), colour_of_a, "a did not move");
        assert_eq!(picker.history.done.len(), history_len, "the edit is still there to try later");
    }

    /// The whole feature, end to end through the public surface: press the button and keep the
    /// colour on the dial, and the frame grows a row showing the colour that was added and asking
    /// what to call it.
    #[test]
    fn adding_a_colour_grows_the_palette_and_the_frame_that_shows_it() {
        with_colour();
        let mut picker = picker(0, "ab");
        assert_eq!(picker.focus(), Focus::Add, "with no swatches the button is the top row");

        let before = render(&picker, 200, 60).len();
        press(&mut picker, &[ENTER, ENTER]);
        let after = render(&picker, 200, 60);

        assert_eq!(after.len(), before + 1, "one new row");
        let swatch = picker.palette().at(0).expect("just added");
        let colour = rgb(swatch.color());
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
