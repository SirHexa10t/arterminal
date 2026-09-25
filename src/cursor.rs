//! Where the cursor is, and where a key sends it.
//!
//! The whole surface is one vertical stack — the palette's swatches, then the `[+]` row, then the
//! canvas — so moving down off the last swatch reaches `[+]`, and down off `[+]` enters the art.
//!
//! # Why this is not a list of focusable rows
//!
//! The sibling project this crate borrows its painter from models the cursor as an index into a
//! `Vec` of every focusable row, rebuilt on each keystroke. That is the right shape THERE: its
//! rows are heterogeneous and sparse, because disabled and filtered options get no entry, so the
//! list is the only thing that knows what exists.
//!
//! Here the rows are dense and rectangular, and rebuilding the list would mean allocating one
//! entry per cell per keypress — twenty thousand of them for a 200x100 canvas, to move the cursor
//! one square. So [`Focus`] IS the cursor, and [`Focus::step`] computes the neighbour directly:
//! constant time, no allocation, and shorter to read than the index arithmetic it replaces.

use crate::canvas::Canvas;
use crate::palette::Palette;

/// Which row, or which cell, the cursor is on.
///
/// `Hash` because it is a coordinate: callers keep sets of visited cells, and so do this
/// module's own tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Focus {
    /// The swatch shown `at` rows down the palette.
    Swatch { at: usize },
    /// The `[+]` row under the palette.
    Add,
    /// A cell of the canvas, counted from its top left.
    Cell { x: usize, y: usize },
}

/// A direction key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    Up,
    Down,
    Left,
    Right,
}

impl Focus {
    /// Where the cursor starts: the first swatch, or `[+]` when there are no swatches yet.
    pub fn first(palette: &Palette) -> Self {
        match palette.is_empty() {
            true => Self::Add,
            false => Self::Swatch { at: 0 },
        }
    }

    /// The neighbour in `dir`, or `None` when there is none — at an edge, or moving sideways in
    /// the palette, which is one column wide.
    ///
    /// `None` means the keystroke changed nothing, which is what lets the caller skip the repaint
    /// entirely rather than redrawing an identical frame.
    ///
    /// THE CURSOR DOES NOT WRAP, and that is a deliberate divergence from the sibling project,
    /// whose form rows wrap top to bottom. A form is a list, where wrapping is a shortcut back to
    /// the start. A canvas is a plane, where position is spatial: holding `up` should stop at the
    /// top edge, not teleport to the bottom one.
    pub fn step(self, dir: Dir, palette: &Palette, canvas: &Canvas) -> Option<Self> {
        match (self, dir) {
            // The palette is one column wide, so sideways means nothing there.
            (Self::Swatch { .. } | Self::Add, Dir::Left | Dir::Right) => None,

            (Self::Swatch { at: 0 }, Dir::Up) => None,
            (Self::Swatch { at }, Dir::Up) => Some(Self::Swatch { at: at - 1 }),
            (Self::Swatch { at }, Dir::Down) => match palette.at(at + 1) {
                Some(_) => Some(Self::Swatch { at: at + 1 }),
                None => Some(Self::Add),
            },

            // `[+]` sits between the two regions, so it is the only row whose neighbours are of
            // different kinds.
            (Self::Add, Dir::Up) => palette.len().checked_sub(1).map(|at| Self::Swatch { at }),
            // Entering the art lands in its top-left corner rather than under wherever the cursor
            // happened to be: `[+]` is a button, not a column, so there is no column to keep.
            (Self::Add, Dir::Down) => canvas_cell(canvas, 0, 0),

            (Self::Cell { x, y }, Dir::Up) => match y.checked_sub(1) {
                Some(above) => canvas_cell(canvas, x, above),
                None => Some(Self::Add),
            },
            (Self::Cell { x, y }, Dir::Down) => canvas_cell(canvas, x, y + 1),
            (Self::Cell { x, y }, Dir::Left) => canvas_cell(canvas, x.checked_sub(1)?, y),
            (Self::Cell { x, y }, Dir::Right) => canvas_cell(canvas, x + 1, y),
        }
    }

    /// Whether this cursor names something that is actually there.
    ///
    /// Nothing in a run removes a row — a palette only grows and the canvas does not resize — so
    /// this is a check on the caller rather than on the cursor: a [`Focus`] built by hand, or one
    /// carried over from a different canvas, should not be able to point at empty space.
    pub fn exists(self, palette: &Palette, canvas: &Canvas) -> bool {
        match self {
            Self::Swatch { at } => palette.at(at).is_some(),
            Self::Add => true,
            Self::Cell { x, y } => canvas.cell(x, y).is_some(),
        }
    }
}

/// `Focus::Cell` for a coordinate that exists, and `None` for one that does not — so every edge
/// of the canvas is refused in one place instead of four.
fn canvas_cell(canvas: &Canvas, x: usize, y: usize) -> Option<Focus> {
    canvas.cell(x, y).map(|_| Focus::Cell { x, y })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::color::Rgb;

    /// Three swatches over a 3-wide, 2-tall canvas: wide enough and tall enough that every edge
    /// case below has an interior to be distinguished from.
    fn surface() -> (Palette, Canvas) {
        let mut palette = Palette::new();
        for (nth, name) in ["one", "two", "three"].into_iter().enumerate() {
            // Distinct colours, because a palette forbids two swatches sharing one.
            palette.push(name, Rgb::new(0, 0, nth as u8)).expect("distinct labels and colours");
        }
        (palette, Canvas::from_text("abc\ndef").expect("valid"))
    }

    /// Walking one key at a time from `from`, collecting where the cursor lands, until it stops.
    fn walk(from: Focus, dir: Dir, palette: &Palette, canvas: &Canvas) -> Vec<Focus> {
        let mut seen = Vec::new();
        let mut at = from;
        while let Some(next) = at.step(dir, palette, canvas) {
            seen.push(next);
            at = next;
            assert!(seen.len() < 100, "step is cycling: {seen:?}");
        }
        seen
    }

    #[test]
    fn the_cursor_starts_on_the_first_swatch() {
        let (palette, _) = surface();
        assert_eq!(Focus::first(&palette), Focus::Swatch { at: 0 });
    }

    #[test]
    fn an_empty_palette_starts_the_cursor_on_the_add_button() {
        assert_eq!(Focus::first(&Palette::new()), Focus::Add);
    }

    /// The property that makes the layout navigable: one direction reaches everything, top to
    /// bottom, crossing from palette to button to art without a special key.
    #[test]
    fn holding_down_walks_the_palette_the_button_and_then_the_whole_canvas() {
        let (palette, canvas) = surface();
        assert_eq!(
            walk(Focus::Swatch { at: 0 }, Dir::Down, &palette, &canvas),
            [
                Focus::Swatch { at: 1 },
                Focus::Swatch { at: 2 },
                Focus::Add,
                Focus::Cell { x: 0, y: 0 },
                Focus::Cell { x: 0, y: 1 },
            ]
        );
    }

    #[test]
    fn holding_up_from_the_art_walks_back_out_through_the_button_to_the_first_swatch() {
        let (palette, canvas) = surface();
        assert_eq!(
            walk(Focus::Cell { x: 2, y: 1 }, Dir::Up, &palette, &canvas),
            [
                Focus::Cell { x: 2, y: 0 },
                Focus::Add,
                Focus::Swatch { at: 2 },
                Focus::Swatch { at: 1 },
                Focus::Swatch { at: 0 },
            ]
        );
    }

    /// Leaving the art upward keeps the column until the last moment, so `up` then `down` from
    /// the top row is not a silent jump across the canvas.
    #[test]
    fn moving_up_inside_the_canvas_keeps_the_column() {
        let (palette, canvas) = surface();
        let landed = Focus::Cell { x: 2, y: 1 }.step(Dir::Up, &palette, &canvas);
        assert_eq!(landed, Some(Focus::Cell { x: 2, y: 0 }));
    }

    #[test]
    fn the_button_leads_into_the_top_left_corner_of_the_art() {
        let (palette, canvas) = surface();
        assert_eq!(Focus::Add.step(Dir::Down, &palette, &canvas), Some(Focus::Cell { x: 0, y: 0 }));
    }

    #[test]
    fn every_cell_of_the_canvas_is_reachable() {
        let (palette, canvas) = surface();
        let mut reached = std::collections::HashSet::new();
        let mut row = Focus::Add;
        while let Some(next) = row.step(Dir::Down, &palette, &canvas) {
            row = next;
            reached.insert(row);
            for cell in walk(row, Dir::Right, &palette, &canvas) {
                reached.insert(cell);
            }
        }
        let expected: std::collections::HashSet<_> = (0..canvas.height())
            .flat_map(|y| (0..canvas.width()).map(move |x| Focus::Cell { x, y }))
            .collect();
        assert_eq!(reached, expected, "every coordinate must be walkable");
    }

    #[test]
    fn the_edges_of_the_canvas_stop_the_cursor_rather_than_wrapping_it() {
        let (palette, canvas) = surface();
        let at = |x, y| Focus::Cell { x, y };
        assert_eq!(at(0, 0).step(Dir::Left, &palette, &canvas), None, "left edge");
        assert_eq!(at(2, 0).step(Dir::Right, &palette, &canvas), None, "right edge");
        assert_eq!(at(0, 1).step(Dir::Down, &palette, &canvas), None, "bottom edge");
    }

    #[test]
    fn the_top_of_the_palette_stops_the_cursor_rather_than_wrapping_it() {
        let (palette, canvas) = surface();
        assert_eq!(Focus::Swatch { at: 0 }.step(Dir::Up, &palette, &canvas), None);
    }

    #[test]
    fn the_palette_is_one_column_so_sideways_does_nothing_there() {
        let (palette, canvas) = surface();
        for row in [Focus::Swatch { at: 1 }, Focus::Add] {
            for dir in [Dir::Left, Dir::Right] {
                assert_eq!(row.step(dir, &palette, &canvas), None, "{row:?} {dir:?}");
            }
        }
    }

    /// With no swatches the `[+]` row is the top of the surface, and `up` has nowhere to go.
    #[test]
    fn an_empty_palette_leaves_the_button_with_nothing_above_it() {
        let canvas = Canvas::from_text("ab").expect("valid");
        let empty = Palette::new();
        assert_eq!(Focus::Add.step(Dir::Up, &empty, &canvas), None);
        assert_eq!(Focus::Add.step(Dir::Down, &empty, &canvas), Some(Focus::Cell { x: 0, y: 0 }));
    }

    /// A one-row, one-cell canvas: the smallest surface, where every neighbour is an edge.
    #[test]
    fn a_single_cell_canvas_has_no_interior_at_all() {
        let canvas = Canvas::from_text("x").expect("valid");
        let empty = Palette::new();
        let only = Focus::Cell { x: 0, y: 0 };
        assert_eq!(only.step(Dir::Left, &empty, &canvas), None);
        assert_eq!(only.step(Dir::Right, &empty, &canvas), None);
        assert_eq!(only.step(Dir::Down, &empty, &canvas), None);
        assert_eq!(
            only.step(Dir::Up, &empty, &canvas),
            Some(Focus::Add),
            "up still leaves the art"
        );
    }

    #[test]
    fn a_cursor_can_say_whether_it_points_at_anything() {
        let (palette, canvas) = surface();
        assert!(Focus::Swatch { at: 2 }.exists(&palette, &canvas));
        assert!(!Focus::Swatch { at: 3 }.exists(&palette, &canvas));
        assert!(Focus::Cell { x: 2, y: 1 }.exists(&palette, &canvas));
        assert!(!Focus::Cell { x: 3, y: 1 }.exists(&palette, &canvas));
        assert!(Focus::Add.exists(&Palette::new(), &canvas), "the button is always there");
    }

    /// Every landing [`Focus::step`] reports must be somewhere the cursor may actually stand —
    /// checked over the whole surface rather than at the handful of points above.
    #[test]
    fn no_step_ever_lands_on_something_that_is_not_there() {
        let (palette, canvas) = surface();
        let everywhere =
            (0..palette.len()).map(|at| Focus::Swatch { at }).chain([Focus::Add]).chain(
                (0..canvas.height())
                    .flat_map(|y| (0..canvas.width()).map(move |x| Focus::Cell { x, y })),
            );
        for from in everywhere {
            for dir in [Dir::Up, Dir::Down, Dir::Left, Dir::Right] {
                if let Some(landed) = from.step(dir, &palette, &canvas) {
                    assert!(landed.exists(&palette, &canvas), "{from:?} {dir:?} -> {landed:?}");
                }
            }
        }
    }
}
