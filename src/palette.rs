//! The palette: the colours a drawing may use, each under a name.
//!
//! # Two things are unique here, and they do different jobs
//!
//! **Colour is identity.** No two swatches may hold the same [`Ink`], because a canvas cell
//! records the colour itself rather than a reference to a swatch — so if two swatches shared a
//! colour, nothing could say which of them a given cell belongs to, and "recolour every cell
//! using this swatch" would have no answer. Uniqueness is what makes that operation meaningful.
//!
//! **A label is a name.** Labels are unique too, and for an unrelated reason: a derived swatch
//! has to name the swatch it follows, and it cannot name it by colour — the colour is the very
//! thing that changes. So the label is the stable handle. [`Palette::rename`] therefore rewrites
//! whoever points at the old name, or the link would break silently.
//!
//! # Why not an opaque id
//!
//! Because a positional one is a trap — remove a swatch and every id after it silently names
//! something else — and an opaque stable one is a field nobody reading the file cares about.
//! A unique label is a stable handle that also happens to mean something.

use crate::color::{Hsb, Ink, Rgb, Rng};
use std::fmt;

/// How a derived swatch is displaced from the one it follows, in the space colours are picked in.
///
/// Kept as an offset rather than as a second colour so the relationship survives the base moving:
/// "the same hue, a third darker" stays true wherever the base goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct HsbOffset {
    /// Steps around the wheel; wraps. See [`Hsb::HUE_STEPS`].
    pub hue: i32,
    /// Added to saturation, clamped at the ends rather than wrapped — a colour one step past
    /// fully grey is still fully grey, not suddenly vivid.
    pub saturation: i32,
    /// Added to brightness, clamped for the same reason.
    pub brightness: i32,
}

impl HsbOffset {
    /// Where `base` lands once this offset is applied.
    pub fn apply(self, base: Hsb) -> Hsb {
        let steps = Hsb::HUE_STEPS as i32;
        Hsb {
            hue: (base.hue as i32 + self.hue).rem_euclid(steps) as u16,
            saturation: (base.saturation as i32 + self.saturation).clamp(0, 255) as u8,
            brightness: (base.brightness as i32 + self.brightness).clamp(0, 255) as u8,
        }
    }
}

/// What a derived swatch follows, and by how much.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Derivation {
    /// The label of the swatch this one follows.
    pub base: String,
    pub offset: HsbOffset,
}

/// One entry of a palette: a colour, what to call it, and whether it follows another.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Swatch {
    label: String,
    color: Ink,
    derivation: Option<Derivation>,
}

impl Swatch {
    pub fn label(&self) -> &str {
        &self.label
    }

    /// The colour as it stands. For a derived swatch this is the RESOLVED colour, already
    /// recomputed — nothing downstream has to know it was derived in order to draw it.
    pub fn color(&self) -> Ink {
        self.color
    }

    pub fn derivation(&self) -> Option<&Derivation> {
        self.derivation.as_ref()
    }
}

/// An ordered set of swatches with unique colours and unique labels.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Palette {
    swatches: Vec<Swatch>,
}

impl Palette {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a swatch of its own colour.
    pub fn push(
        &mut self,
        label: impl Into<String>,
        color: impl Into<Ink>,
    ) -> Result<(), PaletteError> {
        let (label, color) = (label.into(), color.into());
        check_label_shape(&label)?;
        self.check_label_free(&label)?;
        self.check_color_free(color, None)?;
        self.swatches.push(Swatch { label, color, derivation: None });
        Ok(())
    }

    /// Add a swatch of `color` under a generated name, and say what the name was.
    ///
    /// For colours that arrive without one — a drawing loaded from a file that never had a
    /// palette, or a random draw. Names are counted rather than derived from the colour: a label
    /// is a durable handle, and one that was never a description cannot become a lie.
    pub fn push_unnamed(&mut self, color: impl Into<Ink>) -> Result<String, PaletteError> {
        let label = self.free_label();
        self.push(label.clone(), color)?;
        Ok(label)
    }

    /// Take a swatch out, and hand it back.
    ///
    /// Refused while anything still follows it — a derivation names its base by label, and a
    /// dangling one would resolve to nothing the next time its base was asked for. Whether any
    /// CELL still holds the colour is not this type's to know: the picker's undo, the one caller
    /// today, relies on last-in-first-out order to guarantee no paint made with the swatch is
    /// still standing, and any future caller that cannot make that promise has to check the
    /// canvas itself.
    pub fn remove(&mut self, label: &str) -> Result<Swatch, PaletteError> {
        let Some(at) = self.position(label) else {
            return Err(PaletteError::UnknownLabel { label: label.to_string() });
        };
        if let Some(follower) = self
            .swatches
            .iter()
            .find(|swatch| swatch.derivation.as_ref().is_some_and(|d| d.base == label))
        {
            return Err(PaletteError::InUse {
                label: label.to_string(),
                by: follower.label.clone(),
            });
        }
        Ok(self.swatches.remove(at))
    }

    /// Add a swatch that follows `base`, displaced by `offset`.
    ///
    /// Its colour is computed now and recomputed whenever the base moves. A cycle cannot be built
    /// this way and so is not checked for: the new label does not exist until this call succeeds,
    /// so nothing it could follow can already follow it, and [`Palette::rename`] refuses a name
    /// that is taken. The only way to make one would be to repoint an existing derivation, which
    /// no operation here offers.
    pub fn push_derived(
        &mut self,
        label: impl Into<String>,
        base: impl Into<String>,
        offset: HsbOffset,
    ) -> Result<(), PaletteError> {
        let (label, base) = (label.into(), base.into());
        check_label_shape(&label)?;
        self.check_label_free(&label)?;
        let base_color = match self.get(&base).map(Swatch::color) {
            Some(Ink::Rgb(color)) => color,
            Some(Ink::Slot(_)) => return Err(PaletteError::IrregularBase { label: base }),
            None => return Err(PaletteError::UnknownBase { label: base }),
        };
        let color = Ink::Rgb(Rgb::from_hsb(offset.apply(base_color.to_hsb())));
        self.check_color_free(color, None)?;
        self.swatches.push(Swatch { label, color, derivation: Some(Derivation { base, offset }) });
        Ok(())
    }

    /// Add a swatch of a random colour under a generated name.
    ///
    /// Returns the label it chose, since the caller did not supply one.
    ///
    /// A random colour can collide with one already held — [`Rgb::random`] walks a single ring of
    /// the colour wheel, so there are far fewer than sixteen million of them — and a collision is
    /// simply a redraw, not a failure. It gives up after a bounded number of draws rather than
    /// looping for ever on a palette that has taken the whole ring.
    pub fn push_random(&mut self, rng: &mut Rng) -> Result<String, PaletteError> {
        let color = self.free_random_color(rng)?;
        self.push_unnamed(color)
    }

    /// A random colour no swatch here holds — for a new swatch, or for moving an old one.
    ///
    /// Draws a bounded number of times rather than looping: [`Rgb::random`] walks one ring of the
    /// wheel, so a palette that has taken nearly all of it would otherwise spin for ever.
    pub fn free_random_color(&self, rng: &mut Rng) -> Result<Rgb, PaletteError> {
        (0..RANDOM_TRIES)
            .map(|_| Rgb::random(rng))
            .find(|color| self.check_color_free(Ink::Rgb(*color), None).is_ok())
            .ok_or(PaletteError::NoFreeColor)
    }

    /// Move a swatch to a new colour, carrying everything that follows it.
    ///
    /// Returns every `(was, now)` pair the move produced — the base's own, and one for each
    /// derived swatch that shifted with it — so a caller holding pixels can rewrite them without
    /// working out the mapping a second time.
    ///
    /// REFUSED WHOLE if any swatch would land on a colour another already holds, including
    /// collisions between two derived swatches that the caller never mentioned. Nothing is
    /// changed in that case, and the error names the swatch that blocked it: an edit that half
    /// applied would leave the palette in a state its own rules forbid.
    pub fn set_color(
        &mut self,
        label: &str,
        color: impl Into<Ink>,
    ) -> Result<Recolour, PaletteError> {
        let Some(at) = self.position(label) else {
            return Err(PaletteError::UnknownLabel { label: label.to_string() });
        };
        let mut proposed: Vec<Ink> = self.swatches.iter().map(Swatch::color).collect();
        proposed[at] = color.into();
        self.resolve_from(at, &mut proposed)?;

        // Checked against the WHOLE proposal rather than against the palette as it stands: two
        // swatches can be pushed onto each other by the same move, and neither holds the colour
        // yet.
        for (left, colour) in proposed.iter().enumerate() {
            if let Some(right) = proposed.iter().position(|other| other == colour) {
                if right != left {
                    return Err(PaletteError::DuplicateColor {
                        color: *colour,
                        held_by: self.swatches[right.min(left)].label.clone(),
                    });
                }
            }
        }

        let changes = self
            .swatches
            .iter()
            .zip(&proposed)
            .filter(|(swatch, now)| swatch.color != **now)
            .map(|(swatch, now)| (swatch.color, *now))
            .collect();
        for (swatch, now) in self.swatches.iter_mut().zip(proposed) {
            swatch.color = now;
        }
        Ok(Recolour { changes })
    }

    /// Give a swatch a different name, and point everything that followed it at the new one.
    ///
    /// The rewrite is the whole reason this is an operation rather than a field: a label is the
    /// only handle a derivation has, so a rename that did not carry the followers would break
    /// them silently and at a distance.
    pub fn rename(&mut self, from: &str, to: impl Into<String>) -> Result<(), PaletteError> {
        let to = to.into();
        if self.position(from).is_none() {
            return Err(PaletteError::UnknownLabel { label: from.to_string() });
        }
        check_label_shape(&to)?;
        if to != from {
            self.check_label_free(&to)?;
        }
        for swatch in &mut self.swatches {
            if swatch.label == from {
                swatch.label = to.clone();
            }
            if let Some(derivation) = &mut swatch.derivation {
                if derivation.base == from {
                    derivation.base = to.clone();
                }
            }
        }
        Ok(())
    }

    pub fn get(&self, label: &str) -> Option<&Swatch> {
        self.swatches.iter().find(|swatch| swatch.label == label)
    }

    /// The swatch holding `color`, if any. The reverse lookup that colour-as-identity buys.
    pub fn holder_of(&self, color: impl Into<Ink>) -> Option<&Swatch> {
        let color = color.into();
        self.swatches.iter().find(|swatch| swatch.color == color)
    }

    pub fn len(&self) -> usize {
        self.swatches.len()
    }

    pub fn is_empty(&self) -> bool {
        self.swatches.is_empty()
    }

    /// Every swatch, in display order.
    pub fn iter(&self) -> impl ExactSizeIterator<Item = &Swatch> {
        self.swatches.iter()
    }

    /// The swatch shown `at` rows down, which is how a cursor counting screen rows asks for one.
    pub fn at(&self, at: usize) -> Option<&Swatch> {
        self.swatches.get(at)
    }

    fn position(&self, label: &str) -> Option<usize> {
        self.swatches.iter().position(|swatch| swatch.label == label)
    }

    fn check_label_free(&self, label: &str) -> Result<(), PaletteError> {
        match self.position(label) {
            Some(_) => Err(PaletteError::DuplicateLabel { label: label.to_string() }),
            None => Ok(()),
        }
    }

    fn check_color_free(&self, color: Ink, except: Option<usize>) -> Result<(), PaletteError> {
        match self
            .swatches
            .iter()
            .enumerate()
            .find(|(at, swatch)| swatch.color == color && Some(*at) != except)
        {
            Some((_, swatch)) => {
                Err(PaletteError::DuplicateColor { color, held_by: swatch.label.clone() })
            }
            None => Ok(()),
        }
    }

    /// Recompute, in `proposed`, every swatch that follows the one at `at` — and everything that
    /// follows those.
    ///
    /// Iterative rather than recursive over a list the caller controls, so a long chain cannot
    /// overflow the stack. Each swatch is resolved at most once because a derivation forms a
    /// forest: every swatch has at most one base, and cycles cannot be built (see
    /// [`Palette::push_derived`]).
    ///
    /// Refused if a swatch something follows would become a terminal palette slot: see
    /// [`PaletteError::IrregularBase`].
    fn resolve_from(&self, at: usize, proposed: &mut [Ink]) -> Result<(), PaletteError> {
        let mut moved = vec![self.swatches[at].label.clone()];
        while let Some(base) = moved.pop() {
            for (index, swatch) in self.swatches.iter().enumerate() {
                let follows = swatch.derivation.as_ref().is_some_and(|d| d.base == base);
                if !follows {
                    continue;
                }
                let offset = swatch.derivation.as_ref().expect("just matched").offset;
                let from = match proposed[self.position(&base).expect("base exists")] {
                    Ink::Rgb(from) => from,
                    Ink::Slot(_) => return Err(PaletteError::IrregularBase { label: base }),
                };
                proposed[index] = Ink::Rgb(Rgb::from_hsb(offset.apply(from.to_hsb())));
                moved.push(swatch.label.clone());
            }
        }
        Ok(())
    }

    /// A name no swatch is using: `colour 1`, then `colour 2`, and so on.
    ///
    /// Generic on purpose. The obvious alternative — name it after its own hex — is wrong now
    /// that a label is a durable handle: the colour can move, and the label cannot follow it
    /// without breaking every derivation pointing at it. A name that was never a description
    /// cannot become a lie.
    fn free_label(&self) -> String {
        (1..)
            .map(|n| format!("colour {n}"))
            .find(|name| self.position(name).is_none())
            .expect("an unbounded sequence of names cannot be exhausted by a finite palette")
    }
}

/// What a label may be: something, and nothing that would break the line it is written on.
///
/// Labels are saved one per line with tabs between fields, so a tab or a newline inside one
/// would corrupt the file it lands in — and every other control character is invisible on
/// screen, which for a name that is supposed to be read is as good as absent. Anything printable
/// is allowed, spaces included.
fn check_label_shape(label: &str) -> Result<(), PaletteError> {
    let why = if label.is_empty() {
        "a label cannot be empty"
    } else if label.chars().any(char::is_control) {
        "a label cannot contain control characters (tabs and newlines included)"
    } else {
        return Ok(());
    };
    Err(PaletteError::BadLabel { label: label.to_string(), why })
}

/// How many colours [`Palette::push_random`] will draw before admitting the ring is full.
///
/// Generous rather than tuned. [`Rgb::random`] draws from one ring of the wheel — 1530 hues at a
/// fixed saturation and brightness, fewer once rounding collapses neighbours — so a palette with
/// a few hundred swatches still finds a free colour almost at once, and only one that has taken
/// nearly the whole ring runs out. Drawing is a few arithmetic operations, so spending thirty-two
/// of them to avoid a spurious failure costs nothing a person could measure.
const RANDOM_TRIES: usize = 32;

/// What a colour change did: every `(was, now)` pair it produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recolour {
    changes: Vec<(Ink, Ink)>,
}

impl Recolour {
    /// The pairs, in palette order. Empty when the colour asked for was already the colour held.
    pub fn changes(&self) -> &[(Ink, Ink)] {
        &self.changes
    }

    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }
}

/// Why a palette refused something.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaletteError {
    /// Two swatches may not share a colour — see the module docs.
    DuplicateColor { color: Ink, held_by: String },
    /// A swatch cannot follow a terminal palette slot, nor become one while anything follows it.
    /// Not because its colour is unknown — slots 16-255 are defined exactly — but because a
    /// slot's MEANING belongs to the terminal: a derivation computed from it would freeze one
    /// terminal's reading of the slot into the file.
    IrregularBase { label: String },
    /// Two swatches may not share a name, because a derivation names its base by label.
    DuplicateLabel { label: String },
    /// A derivation pointed at a swatch that is not here.
    UnknownBase { label: String },
    /// An operation named a swatch that is not here.
    UnknownLabel { label: String },
    /// Every colour a random draw could reach is already taken.
    NoFreeColor,
    /// A label that is empty or would not survive being written to a file.
    BadLabel { label: String, why: &'static str },
    /// A swatch that cannot be removed because another still follows it.
    InUse { label: String, by: String },
}

impl fmt::Display for PaletteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateColor { color, held_by } => {
                write!(
                    f,
                    "{color} is already held by {held_by:?}, and two swatches cannot share one"
                )
            }
            Self::DuplicateLabel { label } => {
                write!(f, "there is already a swatch called {label:?}")
            }
            Self::UnknownBase { label } => write!(f, "no swatch called {label:?} to follow"),
            Self::UnknownLabel { label } => write!(f, "no swatch called {label:?}"),
            Self::NoFreeColor => write!(f, "every colour a random draw can reach is already taken"),
            Self::IrregularBase { label } => write!(
                f,
                "{label:?} is a terminal palette slot, and a swatch cannot follow one: what a \
                 slot looks like is the terminal's to say"
            ),
            Self::BadLabel { label, why } => write!(f, "{label:?}: {why}"),
            Self::InUse { label, by } => {
                write!(f, "{label:?} cannot be removed while {by:?} still follows it")
            }
        }
    }
}

impl std::error::Error for PaletteError {}

#[cfg(test)]
mod tests {
    use super::*;

    /// The colour of the swatch called `label`, which these tests only ever give colours of their
    /// own.
    fn rgb_of(palette: &Palette, label: &str) -> Rgb {
        palette.get(label).and_then(|swatch| swatch.color().rgb()).expect("a colour of its own")
    }

    fn palette(entries: &[(&str, Rgb)]) -> Palette {
        let mut palette = Palette::new();
        for (label, color) in entries {
            palette.push(*label, *color).expect("the fixture is distinct");
        }
        palette
    }

    const RED: Rgb = Rgb::new(226, 57, 57);
    const BLUE: Rgb = Rgb::new(57, 144, 226);

    #[test]
    fn a_new_palette_is_empty() {
        let palette = Palette::new();
        assert!(palette.is_empty());
        assert_eq!(palette.len(), 0);
        assert_eq!(palette.iter().count(), 0);
    }

    #[test]
    fn pushing_returns_a_swatch_that_reads_back() {
        let palette = palette(&[("ember", RED)]);
        let swatch = palette.get("ember").expect("just pushed");
        assert_eq!(swatch.color(), RED.into());
        assert_eq!(swatch.label(), "ember");
        assert_eq!(swatch.derivation(), None, "pushed with a colour of its own");
    }

    /// The rule the whole design rests on: a canvas cell records a colour, so a colour has to name
    /// exactly one swatch or "recolour every cell using this swatch" has no answer.
    #[test]
    fn two_swatches_may_not_share_a_colour() {
        let mut palette = palette(&[("ember", RED)]);
        assert_eq!(
            palette.push("flame", RED),
            Err(PaletteError::DuplicateColor { color: RED.into(), held_by: "ember".into() })
        );
        assert_eq!(palette.len(), 1, "and nothing was added");
        assert!(palette.push("flame", RED).unwrap_err().to_string().contains("ember"));
    }

    /// And the other uniqueness, for the other reason: a derivation names its base by label.
    #[test]
    fn two_swatches_may_not_share_a_label() {
        let mut palette = palette(&[("ember", RED)]);
        assert_eq!(
            palette.push("ember", BLUE),
            Err(PaletteError::DuplicateLabel { label: "ember".into() })
        );
        assert_eq!(palette.len(), 1);
    }

    #[test]
    fn a_derived_swatch_resolves_to_its_base_displaced() {
        let mut palette = palette(&[("base", RED)]);
        let darker = HsbOffset { brightness: -60, ..HsbOffset::default() };
        palette.push_derived("shade", "base", darker).expect("base exists");

        let expected = Rgb::from_hsb(darker.apply(RED.to_hsb()));
        let swatch = palette.get("shade").expect("just pushed");
        assert_eq!(swatch.color(), expected.into());
        assert_eq!(swatch.derivation().map(|d| d.base.as_str()), Some("base"));
        assert!(swatch.color() != RED.into(), "a derived swatch is still its own colour");
    }

    #[test]
    fn a_derivation_must_name_a_swatch_that_exists() {
        let mut palette = palette(&[("base", RED)]);
        assert_eq!(
            palette.push_derived("shade", "nowhere", HsbOffset::default()),
            Err(PaletteError::UnknownBase { label: "nowhere".into() })
        );
        assert_eq!(palette.len(), 1);
    }

    /// A zero offset would resolve to the base's own colour, which the uniqueness rule forbids —
    /// so "follow it exactly" is not expressible, and that is the rule working rather than a gap.
    #[test]
    fn a_derivation_that_does_not_move_is_refused_as_a_duplicate() {
        let mut palette = palette(&[("base", RED)]);
        assert!(matches!(
            palette.push_derived("twin", "base", HsbOffset::default()),
            Err(PaletteError::DuplicateColor { .. })
        ));
    }

    /// The feature, end to end: move the base and everything following it moves too.
    #[test]
    fn moving_a_base_carries_everything_that_follows_it() {
        let mut palette = palette(&[("base", RED)]);
        let darker = HsbOffset { brightness: -60, ..HsbOffset::default() };
        let darkest = HsbOffset { brightness: -40, ..HsbOffset::default() };
        palette.push_derived("shade", "base", darker).expect("base exists");
        palette.push_derived("deep", "shade", darkest).expect("a chain is allowed");

        let recolour = palette.set_color("base", BLUE).expect("no collision");
        assert_eq!(palette.get("base").unwrap().color(), BLUE.into());
        assert_eq!(
            palette.get("shade").unwrap().color(),
            Rgb::from_hsb(darker.apply(BLUE.to_hsb())).into(),
            "the follower moved with it"
        );
        assert_eq!(
            palette.get("deep").unwrap().color(),
            Rgb::from_hsb(darkest.apply(rgb_of(&palette, "shade").to_hsb())).into(),
            "and so did what follows the follower"
        );
        assert_eq!(recolour.changes().len(), 3, "all three reported: {recolour:?}");
    }

    /// Every pair a caller needs to rewrite pixels with, and nothing it does not.
    #[test]
    fn a_recolour_reports_what_changed_and_only_what_changed() {
        let mut palette = palette(&[("ember", RED), ("sky", BLUE)]);
        let recolour = palette.set_color("ember", Rgb::new(1, 2, 3)).expect("free");
        assert_eq!(recolour.changes(), [(RED.into(), Rgb::new(1, 2, 3).into())]);
        assert!(!recolour.is_empty());

        let nothing = palette.set_color("sky", BLUE).expect("already that colour");
        assert!(nothing.is_empty(), "asking for the colour it already has changes nothing");
    }

    /// Refused WHOLE. A half-applied move would leave the palette in a state its own rules forbid.
    #[test]
    fn a_move_onto_a_taken_colour_is_refused_and_changes_nothing() {
        let mut palette = palette(&[("ember", RED), ("sky", BLUE)]);
        let before = palette.clone();
        assert_eq!(
            palette.set_color("ember", BLUE),
            Err(PaletteError::DuplicateColor { color: BLUE.into(), held_by: "ember".into() })
        );
        assert_eq!(palette, before, "nothing moved");
    }

    /// The collision the caller never mentioned: moving a base can push its FOLLOWER onto a third
    /// swatch. Checked against the whole proposal rather than the palette as it stands, because
    /// neither swatch holds the colour yet at the moment of the check.
    #[test]
    fn a_follower_pushed_onto_another_swatch_also_refuses_the_move() {
        let darker = HsbOffset { brightness: -60, ..HsbOffset::default() };
        let mut palette = palette(&[("base", RED)]);
        palette.push_derived("shade", "base", darker).expect("base exists");
        // A third swatch sitting exactly where `shade` would land if `base` moved to blue.
        let landing = Rgb::from_hsb(darker.apply(BLUE.to_hsb()));
        palette.push("occupied", landing).expect("free for now");
        let before = palette.clone();

        assert!(matches!(
            palette.set_color("base", BLUE),
            Err(PaletteError::DuplicateColor { .. })
        ));
        assert_eq!(palette, before, "the base did not move either");
    }

    #[test]
    fn moving_a_swatch_that_is_not_here_is_an_error() {
        let mut palette = palette(&[("ember", RED)]);
        assert_eq!(
            palette.set_color("nobody", BLUE),
            Err(PaletteError::UnknownLabel { label: "nobody".into() })
        );
    }

    /// A label is the only handle a derivation has, so a rename that did not carry its followers
    /// would break them silently.
    #[test]
    fn renaming_carries_everything_that_pointed_at_the_old_name() {
        let darker = HsbOffset { brightness: -60, ..HsbOffset::default() };
        let mut palette = palette(&[("base", RED)]);
        palette.push_derived("shade", "base", darker).expect("base exists");

        palette.rename("base", "ember").expect("the new name is free");
        assert!(palette.get("base").is_none());
        assert_eq!(palette.get("shade").unwrap().derivation().unwrap().base, "ember");

        // And the link still works, which is the point of carrying it.
        palette.set_color("ember", BLUE).expect("no collision");
        assert_eq!(
            palette.get("shade").unwrap().color(),
            Rgb::from_hsb(darker.apply(BLUE.to_hsb())).into()
        );
    }

    #[test]
    fn renaming_onto_a_taken_name_is_refused() {
        let mut palette = palette(&[("ember", RED), ("sky", BLUE)]);
        let before = palette.clone();
        assert_eq!(
            palette.rename("ember", "sky"),
            Err(PaletteError::DuplicateLabel { label: "sky".into() })
        );
        assert_eq!(palette, before);
        assert_eq!(
            palette.rename("nobody", "anything"),
            Err(PaletteError::UnknownLabel { label: "nobody".into() })
        );
    }

    #[test]
    fn renaming_a_swatch_to_its_own_name_is_allowed_and_does_nothing() {
        let mut palette = palette(&[("ember", RED)]);
        let before = palette.clone();
        palette.rename("ember", "ember").expect("not a collision with itself");
        assert_eq!(palette, before);
    }

    /// Generated names are stable handles, so they are counted rather than derived from the
    /// colour — and they step over any a caller has already used.
    #[test]
    fn generated_names_are_sequential_and_avoid_names_already_taken() {
        let mut rng = Rng::from_seed(7);
        let mut fresh = Palette::new();
        assert_eq!(fresh.push_random(&mut rng).unwrap(), "colour 1");
        assert_eq!(fresh.push_random(&mut rng).unwrap(), "colour 2");

        let mut with_clash = palette(&[("colour 1", RED)]);
        assert_eq!(with_clash.push_random(&mut rng).unwrap(), "colour 2");
    }

    #[test]
    fn random_swatches_never_collide_with_what_is_already_held() {
        let mut rng = Rng::from_seed(99);
        let mut palette = Palette::new();
        for _ in 0..60 {
            palette.push_random(&mut rng).expect("the ring has room");
        }
        let colours: std::collections::HashSet<_> = palette.iter().map(Swatch::color).collect();
        assert_eq!(colours.len(), palette.len(), "every colour distinct");
        let labels: std::collections::HashSet<_> = palette.iter().map(Swatch::label).collect();
        assert_eq!(labels.len(), palette.len(), "every label distinct");
    }

    /// The reverse lookup colour-as-identity buys: given a pixel, which swatch owns it.
    #[test]
    fn a_colour_names_the_one_swatch_holding_it() {
        let palette = palette(&[("ember", RED), ("sky", BLUE)]);
        assert_eq!(palette.holder_of(RED).map(Swatch::label), Some("ember"));
        assert_eq!(palette.holder_of(BLUE).map(Swatch::label), Some("sky"));
        assert_eq!(palette.holder_of(Rgb::new(0, 0, 0)), None);
    }

    #[test]
    fn hue_wraps_and_the_other_two_clamp() {
        let base = Hsb { hue: 10, saturation: 250, brightness: 5 };
        let over = HsbOffset { hue: -20, saturation: 50, brightness: -50 };
        let landed = over.apply(base);
        assert_eq!(landed.hue, Hsb::HUE_STEPS - 10, "hue went round the back");
        assert_eq!(landed.saturation, 255, "saturation stopped at the top");
        assert_eq!(landed.brightness, 0, "brightness stopped at the bottom");
    }

    #[test]
    fn swatches_keep_the_order_they_were_added_in() {
        let palette = palette(&[("a", RED), ("b", BLUE), ("c", Rgb::new(1, 2, 3))]);
        assert_eq!(palette.iter().map(Swatch::label).collect::<Vec<_>>(), ["a", "b", "c"]);
        assert_eq!(palette.at(1).map(Swatch::label), Some("b"));
        assert_eq!(palette.at(3), None);
    }
}

#[cfg(test)]
mod removal_and_naming_tests {
    use super::*;

    const RED: Rgb = Rgb::new(226, 57, 57);
    const BLUE: Rgb = Rgb::new(57, 144, 226);

    #[test]
    fn a_removed_swatch_is_handed_back_and_gone() {
        let mut palette = Palette::new();
        palette.push("ember", RED).unwrap();
        palette.push("sky", BLUE).unwrap();
        let taken = palette.remove("ember").expect("nothing follows it");
        assert_eq!((taken.label(), taken.color()), ("ember", RED.into()));
        assert_eq!(palette.get("ember"), None);
        assert_eq!(palette.len(), 1);
        assert_eq!(
            palette.remove("ember"),
            Err(PaletteError::UnknownLabel { label: "ember".into() }),
            "and cannot be removed twice"
        );
    }

    /// A derivation names its base by label; pulling the base out from under it would leave a
    /// name that resolves to nothing the next time the base moved.
    #[test]
    fn a_swatch_something_follows_cannot_be_removed() {
        let mut palette = Palette::new();
        palette.push("base", RED).unwrap();
        let darker = HsbOffset { brightness: -60, ..HsbOffset::default() };
        palette.push_derived("shade", "base", darker).unwrap();
        assert_eq!(
            palette.remove("base"),
            Err(PaletteError::InUse { label: "base".into(), by: "shade".into() })
        );
        assert_eq!(palette.len(), 2, "nothing was taken");
        palette.remove("shade").expect("the follower itself is free to go");
        palette.remove("base").expect("and then so is the base");
    }

    #[test]
    fn unnamed_colours_get_counted_names() {
        let mut palette = Palette::new();
        assert_eq!(palette.push_unnamed(RED).unwrap(), "colour 1");
        assert_eq!(palette.push_unnamed(BLUE).unwrap(), "colour 2");
        assert!(matches!(palette.push_unnamed(RED), Err(PaletteError::DuplicateColor { .. })));
    }

    /// Labels are written one per line, tab-separated, so a label holding either would corrupt
    /// the file — and an empty one is not a name.
    #[test]
    fn a_label_must_be_something_that_survives_a_file() {
        let mut palette = Palette::new();
        for bad in ["", "tab\there", "two\nlines", "bell\x07"] {
            assert!(
                matches!(palette.push(bad, RED), Err(PaletteError::BadLabel { .. })),
                "{bad:?} should be refused"
            );
        }
        palette.push("spaces are fine", RED).expect("printable, with spaces");
        assert!(matches!(
            palette.rename("spaces are fine", "no\ttabs"),
            Err(PaletteError::BadLabel { .. })
        ));
    }
}
