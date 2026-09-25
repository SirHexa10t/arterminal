//! Colour as this crate means it: 24-bit RGB.
//!
//! Anything narrower loses what the user picked. Terminals that cannot show 24-bit colour are a
//! rendering problem, to be solved by down-converting at the point of drawing — not by making
//! the stored colour smaller, which would throw the original away and could never get it back.

use std::fmt;

/// A 24-bit colour. Small, `Copy`, and comparable, so it can sit inside a palette entry or a
/// canvas cell without ceremony.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    /// Parse `#rrggbb`, or the same six digits without the hash. Case-insensitive.
    ///
    /// Three-digit shorthand (`#f0c`) is deliberately NOT accepted. It can only express colours
    /// whose channels are multiples of 17, so accepting it would mean a file could round-trip
    /// through this crate and come back written differently — and the reader could not tell
    /// whether `#ff00cc` had been shortened or chosen.
    pub fn from_hex(text: &str) -> Result<Self, ColorParseError> {
        let digits = text.strip_prefix('#').unwrap_or(text);
        if digits.len() != 6 || !digits.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(ColorParseError { found: text.to_string() });
        }
        let channel = |at: usize| u8::from_str_radix(&digits[at..at + 2], 16).expect("hex-checked");
        Ok(Self::new(channel(0), channel(2), channel(4)))
    }

    /// The colour at a point on the wheel.
    ///
    /// Integer arithmetic throughout — the classic sextant conversion — so the result is
    /// bit-identical everywhere rather than merely very close.
    pub fn from_hsb(hsb: Hsb) -> Self {
        let (sat, val) = (hsb.saturation as u32, hsb.brightness as u32);
        let hue = hsb.hue % Hsb::HUE_STEPS;
        // Position within the current sextant. It needs no rescaling, which is the whole reason
        // hue is counted in [`Hsb::HUE_STEPS`] rather than degrees — see that constant.
        let offset = (hue % Hsb::SECTOR) as u32;
        let falling = val * (255 - (sat * offset) / 255) / 255;
        let rising = val * (255 - (sat * (255 - offset)) / 255) / 255;
        let bottom = val * (255 - sat) / 255;
        let (r, g, b) = match hue / Hsb::SECTOR {
            0 => (val, rising, bottom),
            1 => (falling, val, bottom),
            2 => (bottom, val, rising),
            3 => (bottom, falling, val),
            4 => (rising, bottom, val),
            _ => (val, bottom, falling),
        };
        // Every branch above is `val`, or `val` scaled down by a fraction of 255, and `val` is a
        // `u8` widened. So all three are already within `u8` and the casts cannot truncate.
        Self::new(r as u8, g as u8, b as u8)
    }

    /// Where on the wheel this colour sits — the inverse of [`Rgb::from_hsb`], as exactly as
    /// integers allow.
    ///
    /// NOT lossless, and the loss is worth knowing because a colour editor round-trips through
    /// here every time one is opened. Saturation is stored as a fraction of brightness
    /// (`chroma * 255 / brightness`), so below full brightness its step is coarser than one unit
    /// of RGB and the return journey can land one off. Measured over all 16,777,216 colours:
    /// 80.7% return exactly, the rest by at most 1 per channel, and EVERY failure has a maximal
    /// channel below 255 — full-brightness colours and greys always survive.
    ///
    /// The practical answer is not to make this exact but not to call it needlessly: an editor
    /// should commit only when the user actually moved something, so a colour opened and closed
    /// untouched never makes the trip at all.
    pub fn to_hsb(self) -> Hsb {
        let (r, g, b) = (self.r as i32, self.g as i32, self.b as i32);
        let max = r.max(g).max(b);
        let min = r.min(g).min(b);
        let chroma = max - min;
        let brightness = max as u8;
        let saturation = match max {
            0 => 0,
            _ => (chroma * 255 / max) as u8,
        };
        // A grey has no direction on the wheel. Hue zero is a convention, not a measurement.
        if chroma == 0 {
            return Hsb { hue: 0, saturation, brightness };
        }
        let sector = Hsb::SECTOR as i32;
        let steps = Hsb::HUE_STEPS as i32;
        let hue = if max == r {
            ((g - b) * 255 / chroma + steps) % steps
        } else if max == g {
            2 * sector + (b - r) * 255 / chroma
        } else {
            4 * sector + (r - g) * 255 / chroma
        };
        Hsb { hue: hue as u16, saturation, brightness }
    }

    /// A random colour worth looking at.
    ///
    /// Sampling `r`, `g` and `b` independently is the obvious thing and it is wrong: the colour
    /// cube's dull interior is vastly larger than its bright, saturated corners, so uniform RGB
    /// returns olive and mud most of the time. Walking the rim of the colour wheel instead — a
    /// uniform HUE at a fixed saturation and brightness — makes consecutive draws reliably produce
    /// colours a person can tell apart, which is the entire point of the button that calls this.
    pub fn random(rng: &mut Rng) -> Self {
        Self::from_hsb(Hsb {
            hue: (rng.next_u64() % Hsb::HUE_STEPS as u64) as u16,
            saturation: SWATCH_SATURATION,
            brightness: SWATCH_VALUE,
        })
    }
}

/// A colour as it is PICKED rather than as it is stored: an angle on the wheel, how much colour,
/// how much light.
///
/// Transient by design. [`Rgb`] is what a swatch and a canvas hold; this exists for the moment a
/// person is choosing, because "a bit more orange" is a thought about hue and an unreachable one
/// about three independent channels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Hsb {
    /// `0..`[`Hsb::HUE_STEPS`], wrapping.
    pub hue: u16,
    pub saturation: u8,
    pub brightness: u8,
}

impl Hsb {
    /// One sixth of the wheel, in hue steps — and 255 of them, one per representable position
    /// within a sextant.
    pub const SECTOR: u16 = 255;

    /// A full turn of the wheel: six sextants of [`Hsb::SECTOR`].
    ///
    /// NOT 360, and the difference is measurable rather than tidy. In degrees, a sextant holds
    /// sixty distinct hues that must then be spread across a range of 255 — so 195 of every 255
    /// positions inside it are unreachable, and integer HSB can name only 58.6% of the 16.7M
    /// colours. Round-tripping a colour through a degree-based editor changed it two times in
    /// three, by as much as 5 per channel. At 1530 steps nothing is spread and nothing is lost
    /// there: 80.7% of colours return exactly and the rest by at most 1, with the entire residual
    /// coming from saturation (see [`Rgb::to_hsb`]).
    ///
    /// Show degrees if degrees read better — [`Hsb::degrees`] — but do not store them.
    pub const HUE_STEPS: u16 = 6 * Self::SECTOR;

    /// This hue in degrees, for display. Lossy, and only ever for a label.
    pub fn degrees(self) -> u16 {
        (self.hue as u32 * 360 / Self::HUE_STEPS as u32) as u16
    }
}

/// Saturation and value for a generated swatch.
///
/// Not `255`/`255`. Fully saturated primaries at full brightness vibrate against each other when
/// several sit in a column, and leave no headroom above them for a highlight later. Backing both
/// off a little keeps generated swatches distinguishable side by side and still usable as a base
/// colour. Chosen by eye against the layout in [`crate::ui`]; nothing downstream depends on the
/// exact values, so they are safe to retune.
const SWATCH_SATURATION: u8 = 191; // ~75%
const SWATCH_VALUE: u8 = 230; // ~90%

/// `#rrggbb`, lower case — the form [`Rgb::from_hex`] accepts, so the two round-trip.
impl fmt::Display for Rgb {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "#{:02x}{:02x}{:02x}", self.r, self.g, self.b)
    }
}

/// What a cell is inked with, and what a swatch holds: a colour of its own, or one of the
/// terminal's palette SLOTS — the 16 and 256 colours an escape can name by number instead.
///
/// A slot is not a colour but a place in the terminal's palette. The first sixteen are the
/// theme's, and a terminal can be told to change any of the 256. So a slot is kept AS A SLOT:
/// drawn through the terminal, which shows it exactly as this terminal shows it, and written back
/// as the code that named it — never pinned to an RGB it might not have. [`Ink::approximate`]
/// gives the RGB it usually is, for the few decisions that need one, like which of black or white
/// stands out on it.
///
/// Colour is identity for both kinds, and a slot never equals a colour: slot 196 and `#ff0000`
/// look alike on most terminals, but only one of them follows the terminal's palette, so they are
/// two colours, and can be two swatches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Ink {
    /// A colour of its own, the same everywhere.
    Rgb(Rgb),
    /// A slot of the terminal's 256-colour palette.
    Slot(u8),
}

impl From<Rgb> for Ink {
    fn from(rgb: Rgb) -> Self {
        Self::Rgb(rgb)
    }
}

impl Ink {
    /// The RGB this ink is: exactly, for a colour of its own; for a slot, what xterm's default
    /// table holds there — exact for 16-255, a guess for the theme's 0-15. See [`palette_colour`].
    pub fn approximate(self) -> Rgb {
        match self {
            Self::Rgb(rgb) => rgb,
            Self::Slot(slot) => palette_colour(slot),
        }
    }

    /// The colour, when this is one of its own; `None` for a slot.
    pub fn rgb(self) -> Option<Rgb> {
        match self {
            Self::Rgb(rgb) => Some(rgb),
            Self::Slot(_) => None,
        }
    }

    /// Whether this is one of the sixteen slots that take the terminal THEME's colours, and so
    /// can look different on every terminal. Slots 16-255 are defined exactly.
    pub fn follows_theme(self) -> bool {
        matches!(self, Self::Slot(0..=15))
    }

    /// Read `#rrggbb` — the hash optional, either case — or `slot 196`, as [`Ink`]'s `Display`
    /// writes them. A slot outside 0-255 is refused, not wrapped or clamped.
    pub fn parse(text: &str) -> Result<Self, ColorParseError> {
        match text.strip_prefix("slot") {
            Some(number) => number
                .trim_start()
                .parse::<u8>()
                .map(Self::Slot)
                .map_err(|_| ColorParseError { found: text.to_string() }),
            None => Rgb::from_hex(text).map(Self::Rgb),
        }
    }
}

/// `#rrggbb` for a colour of its own, `slot 196` for a slot — the spelling a palette line uses.
impl fmt::Display for Ink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Rgb(rgb) => rgb.fmt(f),
            Self::Slot(slot) => write!(f, "slot {slot}"),
        }
    }
}

/// The colour a terminal's 256-colour palette holds in slot `index`, by xterm's default table.
///
/// Slots 16-255 are defined exactly and every terminal agrees on them: a 6×6×6 cube on the levels
/// 0, 95, 135, 175, 215 and 255, then a ramp of greys from 8 to 238 in steps of 10. Slots 0-15
/// are the theme's own sixteen colours, which differ from terminal to terminal and are usually
/// changed by the user; xterm's defaults stand in for them here — which are NOT the old VGA values
/// some terminals ship — so for those this is a best guess, and is called one.
pub fn palette_colour(index: u8) -> Rgb {
    const SIXTEEN: [(u8, u8, u8); 16] = [
        (0, 0, 0),
        (205, 0, 0),
        (0, 205, 0),
        (205, 205, 0),
        (0, 0, 238),
        (205, 0, 205),
        (0, 205, 205),
        (229, 229, 229),
        (127, 127, 127),
        (255, 0, 0),
        (0, 255, 0),
        (255, 255, 0),
        (92, 92, 255),
        (255, 0, 255),
        (0, 255, 255),
        (255, 255, 255),
    ];
    const LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];
    match index {
        0..=15 => {
            let (r, g, b) = SIXTEEN[index as usize];
            Rgb::new(r, g, b)
        }
        16..=231 => {
            let cube = index - 16;
            let (r, g, b) = (cube / 36, cube / 6 % 6, cube % 6);
            Rgb::new(LEVELS[r as usize], LEVELS[g as usize], LEVELS[b as usize])
        }
        232..=255 => {
            let grey = 8 + 10 * (index - 232);
            Rgb::new(grey, grey, grey)
        }
    }
}

/// What [`Rgb::from_hex`] rejected, kept whole so the message can quote it back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColorParseError {
    found: String,
}

impl fmt::Display for ColorParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "expected a colour like #1e90ff or slot 196, found {:?}", self.found)
    }
}

impl std::error::Error for ColorParseError {}

/// A small, seedable generator — enough to choose a colour, and nothing more.
///
/// This is SplitMix64: the algorithm behind Java's `SplittableRandom`, and the one the xoshiro
/// authors recommend for seeding their own generators. Sixteen lines of shifts and multiplies
/// with no dependency, and — the reason it is here rather than a crate — deterministic from a
/// seed, which is what makes [`Rgb::random`] testable at all.
///
/// Rejected: the `rand` crate. It would pull four transitive crates into a tree that otherwise
/// has two, to produce one `u64` for a placeholder feature.
///
/// NOT cryptographic, and nothing here needs it to be. Do not reach for this to make a secret.
#[derive(Debug, Clone)]
pub struct Rng(u64);

impl Rng {
    /// Seeded from the operating system, via the standard library rather than a crate.
    ///
    /// `RandomState` exists to make `HashMap` resistant to collision attacks, which means std
    /// already owns "obtain entropy from the system" and solves it on every platform it supports.
    /// Hashing nothing with a fresh one yields that key material. Rejected: seeding from the
    /// clock, which is guessable and repeats across a fast restart.
    pub fn from_entropy() -> Self {
        use std::collections::hash_map::RandomState;
        use std::hash::{BuildHasher, Hasher};
        Self(RandomState::new().build_hasher().finish())
    }

    /// A generator that will produce the same sequence every time — for tests, and for any caller
    /// that wants a palette it can reproduce.
    pub const fn from_seed(seed: u64) -> Self {
        Self(seed)
    }

    /// The next value in the sequence.
    ///
    /// The three constants are SplitMix64's own: the increment is the 64-bit golden-ratio
    /// fraction, and the two multipliers are the finalizer's. They are not tunable — changing
    /// any of them makes this a different, unstudied generator.
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_round_trips_through_display_and_back() {
        for colour in [Rgb::new(0, 0, 0), Rgb::new(255, 255, 255), Rgb::new(30, 144, 255)] {
            assert_eq!(Rgb::from_hex(&colour.to_string()), Ok(colour));
        }
    }

    #[test]
    fn hex_is_accepted_with_or_without_the_hash_and_in_either_case() {
        let expected = Ok(Rgb::new(0xAB, 0xCD, 0xEF));
        assert_eq!(Rgb::from_hex("#abcdef"), expected);
        assert_eq!(Rgb::from_hex("abcdef"), expected);
        assert_eq!(Rgb::from_hex("#ABCDEF"), expected);
        assert_eq!(Rgb::from_hex("#AbCdEf"), expected);
    }

    #[test]
    fn hex_rejects_anything_that_is_not_six_digits() {
        for bad in ["", "#", "#abc", "#abcde", "#abcdefa", "#abcdeg", "##abcdef", "rebeccapurple"] {
            assert!(Rgb::from_hex(bad).is_err(), "{bad:?} should not parse");
        }
    }

    /// Shorthand is refused rather than expanded — see [`Rgb::from_hex`]. Pinned because
    /// "helpfully" accepting it later would silently change what a file means.
    #[test]
    fn three_digit_shorthand_is_not_quietly_accepted() {
        assert!(Rgb::from_hex("#f0c").is_err());
    }

    /// The error names what it saw, so a message built from it can point at the offending text
    /// rather than merely announcing that something somewhere was wrong.
    #[test]
    fn the_parse_error_quotes_what_it_rejected() {
        let err = Rgb::from_hex("#nope").unwrap_err();
        assert!(err.to_string().contains("#nope"), "{err}");
    }

    fn hsb(hue: u16, saturation: u8, brightness: u8) -> Hsb {
        Hsb { hue, saturation, brightness }
    }

    /// The six sextant corners, which are where the branch arithmetic is easiest to get wrong.
    #[test]
    fn full_saturation_and_brightness_give_the_primaries_and_their_mixes() {
        let corner = |sextant: u16| Rgb::from_hsb(hsb(sextant * Hsb::SECTOR, 255, 255));
        assert_eq!(corner(0), Rgb::new(255, 0, 0), "red");
        assert_eq!(corner(1), Rgb::new(255, 255, 0), "yellow");
        assert_eq!(corner(2), Rgb::new(0, 255, 0), "green");
        assert_eq!(corner(3), Rgb::new(0, 255, 255), "cyan");
        assert_eq!(corner(4), Rgb::new(0, 0, 255), "blue");
        assert_eq!(corner(5), Rgb::new(255, 0, 255), "magenta");
    }

    #[test]
    fn hue_wraps_all_the_way_round() {
        let turn = Hsb::HUE_STEPS;
        assert_eq!(Rgb::from_hsb(hsb(turn, 255, 255)), Rgb::from_hsb(hsb(0, 255, 255)), "one turn");
        assert_eq!(
            Rgb::from_hsb(hsb(2 * turn + 5, 255, 255)),
            Rgb::from_hsb(hsb(5, 255, 255)),
            "two turns and five steps"
        );
    }

    #[test]
    fn no_saturation_is_grey_and_no_brightness_is_black() {
        for hue in (0..Hsb::HUE_STEPS).step_by(157) {
            let grey = Rgb::from_hsb(hsb(hue, 0, 200));
            assert_eq!((grey.r, grey.g), (grey.g, grey.b), "hue {hue} should not tint a grey");
            assert_eq!(Rgb::from_hsb(hsb(hue, 255, 0)), Rgb::new(0, 0, 0), "hue {hue} unlit");
        }
    }

    /// Degrees are for reading, not for storing — a full turn is still a full turn.
    #[test]
    fn hue_can_be_shown_in_degrees() {
        assert_eq!(hsb(0, 0, 0).degrees(), 0);
        assert_eq!(hsb(Hsb::SECTOR, 0, 0).degrees(), 60, "one sextant is sixty degrees");
        assert_eq!(hsb(3 * Hsb::SECTOR, 0, 0).degrees(), 180);
        assert_eq!(hsb(Hsb::HUE_STEPS - 1, 0, 0).degrees(), 359);
    }

    /// The invariant a colour editor rests on, stated as the contract rather than as a hope: the
    /// colours that survive a round trip EXACTLY are those at full brightness, and every grey.
    /// Those are also the ones a palette is mostly made of.
    #[test]
    fn full_brightness_colours_and_greys_survive_the_round_trip_exactly() {
        for r in (0..=255u16).step_by(5) {
            for g in (0..=255u16).step_by(5) {
                let bright = Rgb::new(255, r as u8, g as u8);
                assert_eq!(Rgb::from_hsb(bright.to_hsb()), bright, "{bright} at full brightness");
                let grey = Rgb::new(r as u8, r as u8, r as u8);
                assert_eq!(Rgb::from_hsb(grey.to_hsb()), grey, "{grey} is grey");
            }
        }
    }

    /// And the bound on everything else. Dim colours can come back one off, because saturation is
    /// a fraction of brightness and so has a coarser step than RGB below full brightness — but
    /// NEVER more than one, which is what makes the loss a rounding detail rather than a visible
    /// colour change. That bound is the contract; the 80.7%-exact figure in [`Rgb::to_hsb`] is a
    /// measurement over the whole cube and deliberately not asserted here, because any sample
    /// small enough to run in a unit test gives a different percentage — striding by 7 never
    /// reaches 255, for instance, and so counts only the colours that lose.
    #[test]
    fn every_other_colour_returns_within_one_step_per_channel() {
        for r in (0..=255u16).step_by(5) {
            for g in (0..=255u16).step_by(15) {
                for b in (0..=255u16).step_by(17) {
                    let colour = Rgb::new(r as u8, g as u8, b as u8);
                    let back = Rgb::from_hsb(colour.to_hsb());
                    let off = |a: u8, b: u8| (a as i32 - b as i32).abs();
                    let worst =
                        off(back.r, colour.r).max(off(back.g, colour.g)).max(off(back.b, colour.b));
                    assert!(worst <= 1, "{colour} came back as {back}, {worst} off");
                }
            }
        }
    }

    /// Hue is a measurement of direction, and a grey has none. Zero is the convention, so that
    /// the same grey always reads back the same way rather than keeping a hue nobody can see.
    #[test]
    fn a_grey_reports_no_hue_rather_than_an_arbitrary_one() {
        for level in [0u8, 1, 128, 254, 255] {
            let hsb = Rgb::new(level, level, level).to_hsb();
            assert_eq!(hsb.hue, 0, "grey {level}");
            assert_eq!(hsb.saturation, 0, "grey {level}");
            assert_eq!(hsb.brightness, level, "grey {level}");
        }
    }

    /// A seeded generator is reproducible, which is the property that makes everything above it
    /// testable; two different seeds are not.
    #[test]
    fn seeded_generators_repeat_and_differ() {
        let draw = |seed| {
            let mut rng = Rng::from_seed(seed);
            (0..8).map(|_| Rgb::random(&mut rng)).collect::<Vec<_>>()
        };
        assert_eq!(draw(42), draw(42), "the same seed gives the same palette");
        assert_ne!(draw(42), draw(43), "a different seed gives a different one");
    }

    /// The reason [`Rgb::random`] walks the colour wheel instead of the cube: consecutive
    /// presses of the button have to give colours a person can tell apart.
    #[test]
    fn random_colours_are_vivid_rather_than_muddy() {
        let mut rng = Rng::from_seed(0xC0FFEE);
        let mut seen = std::collections::HashSet::new();
        for _ in 0..64 {
            let colour = Rgb::random(&mut rng);
            let channels = [colour.r, colour.g, colour.b];
            let spread = channels.iter().max().unwrap() - channels.iter().min().unwrap();
            assert!(spread > 64, "{colour} is too close to grey to be a useful swatch");
            assert!(*channels.iter().max().unwrap() > 128, "{colour} is too dark to read");
            seen.insert(colour);
        }
        assert!(seen.len() > 50, "64 draws gave only {} distinct colours", seen.len());
    }

    /// An ink is written the way a palette line spells it, and read back from that spelling: a
    /// colour of its own as hex, a slot as `slot n`. A slot past 255 is refused, not wrapped.
    #[test]
    fn an_ink_round_trips_through_its_spelling() {
        for ink in [Ink::Rgb(Rgb::new(30, 144, 255)), Ink::Slot(0), Ink::Slot(196), Ink::Slot(255)]
        {
            assert_eq!(Ink::parse(&ink.to_string()), Ok(ink), "{ink}");
        }
        assert_eq!(Ink::Slot(24).to_string(), "slot 24");
        assert_eq!(Ink::parse("slot24"), Ok(Ink::Slot(24)), "the space is optional");
        let refused = Ink::parse("slot 256").expect_err("no slot 256");
        assert!(refused.to_string().contains("slot 196"), "and says what would do: {refused}");
    }

    /// A slot is never a colour of its own, whatever it looks like — only one of them follows
    /// the terminal's palette — and only the first sixteen follow the theme.
    #[test]
    fn a_slot_is_its_own_identity_and_only_sixteen_follow_the_theme() {
        assert_ne!(Ink::Slot(196), Ink::Rgb(palette_colour(196)), "alike, and still two colours");
        assert_eq!(Ink::Slot(196).approximate(), Rgb::new(255, 0, 0));
        assert_eq!(Ink::Rgb(Rgb::new(1, 2, 3)).approximate(), Rgb::new(1, 2, 3), "exact");
        assert!(Ink::Slot(15).follows_theme() && !Ink::Slot(16).follows_theme());
        assert!(!Ink::Rgb(Rgb::new(0, 0, 0)).follows_theme());
    }

    /// The palette slots, from xterm's default table: 16-255 are exact and agreed everywhere;
    /// 0-15 are the theme's, and xterm's defaults stand in for them.
    #[test]
    fn palette_slots_map_to_xterms_default_table() {
        for (index, rgb) in [
            (1, (205, 0, 0)),
            (9, (255, 0, 0)),
            (16, (0, 0, 0)),
            (21, (0, 0, 255)),
            (196, (255, 0, 0)),
            (231, (255, 255, 255)),
            (232, (8, 8, 8)),
            (255, (238, 238, 238)),
        ] {
            assert_eq!(palette_colour(index), Rgb::new(rgb.0, rgb.1, rgb.2), "slot {index}");
        }
    }
}
