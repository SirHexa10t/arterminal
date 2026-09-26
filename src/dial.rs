//! The colour dial: a colour chosen by hue, saturation and brightness, one key at a time.
//!
//! Pure, like everything else that decides what appears: [`crate::ui`] draws it and hands it the
//! keys, and everything about turning it is tested here without a terminal.
//!
//! # Shown one way, held another
//!
//! Hue is SHOWN in whole degrees and saturation and brightness in whole percent, because those
//! are the numbers a person reads — and one press moves one of them by exactly one. The colour is
//! HELD as an [`Hsb`] at full precision: hue in [`Hsb::HUE_STEPS`], the other two in 256 steps.
//!
//! That extra resolution is for FILES, not for the dial. Turning reaches only the colours on the
//! shown grid, 360 hues by 101 by 101, and that is deliberate: a person steps in the units they
//! read. Do not "fix" the storage to match what the dial can reach — [`Hsb::HUE_STEPS`] records
//! what a store in degrees cost, measured.
//!
//! What keeps the two honest is that a dial remembers where it started. A channel is moved onto
//! the shown grid only while it shows something other than what it showed at first; turned back
//! to that, it holds exactly what it held. So a dial only looked at, or turned and turned back,
//! hands back its colour to the bit — even a colour from a file that sits between the grid's
//! points — and a visit that changed nothing recolours nothing.

use crate::color::{Hsb, Rgb};

/// Which of the three the dial is turning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channel {
    Hue,
    Saturation,
    Brightness,
}

impl Channel {
    /// All three, in the order they are shown.
    pub const ALL: [Channel; 3] = [Channel::Hue, Channel::Saturation, Channel::Brightness];

    /// The letter it is shown under.
    pub fn letter(self) -> char {
        match self {
            Channel::Hue => 'H',
            Channel::Saturation => 'S',
            Channel::Brightness => 'B',
        }
    }

    /// The top of its shown range: 360 degrees, which is where hue comes round to 0 again, and
    /// 100 percent.
    fn top(self) -> u16 {
        match self {
            Channel::Hue => 360,
            Channel::Saturation | Channel::Brightness => 100,
        }
    }
}

/// A colour being chosen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Dial {
    /// The colour it was given, exactly.
    start: Rgb,
    /// What each channel goes back to holding when it is turned back to the value it showed at
    /// first — see the module docs.
    from: Hsb,
    hsb: Hsb,
    channel: Channel,
}

impl Dial {
    /// A dial set to `start`, turning its hue.
    pub fn new(start: Rgb) -> Self {
        let from = start.to_hsb();
        Self { start, from, hsb: from, channel: Channel::Hue }
    }

    /// The colour as dialled — exactly the colour it was given, while every channel shows what
    /// it did at first.
    pub fn rgb(&self) -> Rgb {
        match self.hsb == self.from {
            true => self.start,
            false => Rgb::from_hsb(self.hsb),
        }
    }

    /// The colour it was given.
    pub fn start(&self) -> Rgb {
        self.start
    }

    /// Whether the colour is not the one it was given. Not whether anything was TURNED: a hue
    /// turned on a grey changes no colour, and neither does a channel turned and turned back.
    pub fn is_changed(&self) -> bool {
        self.rgb() != self.start
    }

    /// The channel the arrows turn.
    pub fn channel(&self) -> Channel {
        self.channel
    }

    /// Move to the channel `by` places along — Left is -1, Right +1 — stopping at either end, as
    /// the cursor does. `false` if it was already there.
    pub fn select(&mut self, by: i32) -> bool {
        let at = Channel::ALL.iter().position(|c| *c == self.channel).expect("one of the three");
        let to = (at as i32 + by).clamp(0, Channel::ALL.len() as i32 - 1) as usize;
        self.channel = Channel::ALL[to];
        to != at
    }

    /// Turn the channel by `by` of its shown steps — degrees, or percent. Hue goes round;
    /// saturation and brightness stop at 0 and 100. `false` if nothing moved.
    pub fn turn(&mut self, by: i32) -> bool {
        let shown = self.shown(self.channel) as i32 + by;
        let to = match self.channel {
            Channel::Hue => shown.rem_euclid(Channel::Hue.top() as i32),
            _ => shown.clamp(0, self.channel.top() as i32),
        };
        let before = self.hsb;
        self.hsb = set_shown(self.hsb, self.channel, to as u16, self.from);
        self.hsb != before
    }

    /// A channel as shown: hue in degrees, 0-359; saturation and brightness in percent, 0-100.
    pub fn shown(&self, channel: Channel) -> u16 {
        shown_of(self.hsb, channel)
    }

    /// Whether turning `channel` would change the colour at all. Hue means nothing without
    /// saturation and light, and saturation nothing without light: on a grey the hue is only
    /// where the colour will go once saturation is turned up — kept, so that it goes back there.
    pub fn in_effect(&self, channel: Channel) -> bool {
        let lit = self.shown(Channel::Brightness) > 0;
        match channel {
            Channel::Hue => lit && self.shown(Channel::Saturation) > 0,
            Channel::Saturation => lit,
            Channel::Brightness => true,
        }
    }

    /// The colours a strip for `channel` shows, `cells` of them from the top — its highest value
    /// — down to the bottom, its lowest, in even jumps.
    ///
    /// Saturation and brightness are slices through the colour as it is: the other two held
    /// where they are, so every cell is a colour one turn could reach. Hue is not: its strip is
    /// the PURE hues, at full saturation and brightness, whatever the colour's own are — as hue
    /// sliders usually are, and for their reason. Sliced through a grey, the hue strip would be
    /// one flat grey, and a marker moving over it would say nothing about where the hue had gone.
    pub fn strip(&self, channel: Channel, cells: usize) -> Vec<Rgb> {
        (0..cells)
            .map(|cell| {
                let value = value_at(channel, cell, cells);
                let hsb = match channel {
                    Channel::Hue => Hsb {
                        hue: hue_at(value % Channel::Hue.top()),
                        saturation: u8::MAX,
                        brightness: u8::MAX,
                    },
                    Channel::Saturation => Hsb { saturation: from_percent(value), ..self.hsb },
                    Channel::Brightness => Hsb { brightness: from_percent(value), ..self.hsb },
                };
                Rgb::from_hsb(hsb)
            })
            .collect()
    }

    /// Which cell, from the top, of a strip `cells` tall shows the value nearest `channel`'s —
    /// the one the marker stands beside. A hue of 0 is at the BOTTOM: the strip runs from 360 at
    /// the top down to 0, and both ends are the same red, so the bottom is where counting up from
    /// nothing starts.
    pub fn marker(&self, channel: Channel, cells: usize) -> usize {
        let (top, span) = (channel.top() as usize, cells.max(2) - 1);
        let below_top = (top - self.shown(channel) as usize) * span;
        ((below_top + top / 2) / top).min(cells.saturating_sub(1))
    }
}

/// `channel` of `hsb` as shown. Hue by [`Hsb::degrees`], the crate's one conversion to degrees.
fn shown_of(hsb: Hsb, channel: Channel) -> u16 {
    match channel {
        Channel::Hue => hsb.degrees(),
        Channel::Saturation => percent(hsb.saturation),
        Channel::Brightness => percent(hsb.brightness),
    }
}

/// `hsb` with `channel` set to show `value`: what `from` held, when `from` showed that value too,
/// and otherwise a held value that shows as it.
fn set_shown(hsb: Hsb, channel: Channel, value: u16, from: Hsb) -> Hsb {
    let back = value == shown_of(from, channel);
    match channel {
        Channel::Hue => Hsb { hue: if back { from.hue } else { hue_at(value) }, ..hsb },
        Channel::Saturation => {
            Hsb { saturation: if back { from.saturation } else { from_percent(value) }, ..hsb }
        }
        Channel::Brightness => {
            Hsb { brightness: if back { from.brightness } else { from_percent(value) }, ..hsb }
        }
    }
}

/// The value a strip `cells` tall shows at `cell` from the top: its channel's top at the top, 0
/// at the bottom, in even jumps, rounded to what can be shown.
fn value_at(channel: Channel, cell: usize, cells: usize) -> u16 {
    let (top, span) = (channel.top() as usize, cells.max(2) - 1);
    ((top * (span - cell.min(span)) + span / 2) / span) as u16
}

/// The first hue step that [`Hsb::degrees`] shows as `degrees`. First, because that conversion
/// truncates: a degree is 4.25 steps, and rounding here instead would land a quarter of them on
/// the degree below — a press that did not move the number.
fn hue_at(degrees: u16) -> u16 {
    let steps = Hsb::HUE_STEPS as u32;
    ((degrees as u32 * steps).div_ceil(360) % steps) as u16
}

/// A 0-255 channel as the nearest whole percent.
fn percent(value: u8) -> u16 {
    ((value as u32 * 100 + 127) / 255) as u16
}

/// A whole percent as the nearest 0-255 channel — which [`percent`] shows as that percent again.
fn from_percent(percent: u16) -> u8 {
    ((percent as u32 * 255 + 50) / 100) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Colours that do not survive the trip through [`Hsb`] — see [`Rgb::to_hsb`] — beside ones
    /// that do. Tests over these prove the dial hands back what it was given, not merely what
    /// the trip happens to preserve.
    fn awkward() -> Vec<Rgb> {
        let colours = vec![
            Rgb::new(10, 20, 31),
            Rgb::new(200, 100, 7),
            Rgb::new(30, 144, 255),
            Rgb::new(9, 9, 9),
        ];
        assert!(
            colours.iter().any(|c| Rgb::from_hsb(c.to_hsb()) != *c),
            "at least one of these must be a colour the trip changes, or the tests prove nothing"
        );
        colours
    }

    fn dial(r: u8, g: u8, b: u8) -> Dial {
        Dial::new(Rgb::new(r, g, b))
    }

    // ---- what it hands back -------------------------------------------------------------------

    /// A dial only looked at gives back exactly the colour it was given — every bit of it, even
    /// for a colour the trip through hue, saturation and brightness would have changed.
    #[test]
    fn an_untouched_dial_gives_back_exactly_what_it_was_given() {
        for rgb in awkward() {
            let mut d = Dial::new(rgb);
            d.select(1);
            d.select(-1);
            assert_eq!(d.rgb(), rgb);
            assert!(!d.is_changed(), "{rgb}");
        }
    }

    /// Turned away and back, on any channel, is no change at all: the channel holds what it held
    /// before, not the nearest point of the shown grid.
    #[test]
    fn turning_away_and_back_is_no_change_at_all() {
        for rgb in awkward() {
            for (at, channel) in Channel::ALL.into_iter().enumerate() {
                let mut d = Dial::new(rgb);
                d.select(at as i32);
                for there in [1, -1, 7, -7, 180] {
                    // Back by as far as the number really went: a turn can stop at an end.
                    let before = d.shown(channel) as i32;
                    d.turn(there);
                    d.turn(before - d.shown(channel) as i32);
                    assert_eq!(d.rgb(), rgb, "{rgb}, {channel:?} by {there:+} and back");
                    assert!(!d.is_changed());
                }
            }
        }
    }

    /// A colour is changed when it LOOKS different, not when a number moved: a hue turned on a
    /// grey moves no colour at all.
    #[test]
    fn a_hue_turned_on_a_grey_is_no_change() {
        let mut grey = dial(128, 128, 128);
        assert!(grey.turn(90), "the hue did move");
        assert_eq!(grey.shown(Channel::Hue), 90);
        assert!(!grey.is_changed(), "but a grey has no hue to show");
        grey.select(1);
        grey.turn(50);
        assert!(grey.is_changed(), "saturation is a change");
    }

    // ---- turning --------------------------------------------------------------------------------

    /// Hue is shown in degrees and goes round; saturation and brightness in percent, and stop.
    #[test]
    fn hue_goes_round_and_the_other_two_stop_at_their_ends() {
        let mut d = dial(255, 0, 0);
        assert_eq!(Channel::ALL.map(|c| d.shown(c)), [0, 100, 100], "pure red");
        assert!(d.turn(-1), "below 0 degrees");
        assert_eq!(d.shown(Channel::Hue), 359, "is 359");
        assert!(d.turn(1));
        assert_eq!(d.shown(Channel::Hue), 0, "and back round");
        assert!(d.turn(365));
        assert_eq!(d.shown(Channel::Hue), 5, "a long turn goes round too");
        d.select(1);
        assert!(!d.turn(5), "saturation already at 100");
        assert!(d.turn(-150), "and all the way down");
        assert_eq!(d.shown(Channel::Saturation), 0, "stops at 0");
        assert!(!d.turn(-1), "and stays there");
    }

    /// Every press moves the number by exactly one: every shown value is reached, none skipped
    /// or shown twice — the reason a degree is not simply rounded to the nearest hue step.
    #[test]
    fn every_press_moves_the_number_by_exactly_one() {
        let mut d = dial(255, 0, 0);
        for degree in 0..360 {
            assert_eq!(d.shown(Channel::Hue), degree);
            assert!(d.turn(1));
        }
        d.select(2);
        d.turn(-100);
        for percent in 0..=100 {
            assert_eq!(d.shown(Channel::Brightness), percent);
            d.turn(1);
        }
    }

    /// Left and right move between the three and stop at either end, as the cursor does.
    #[test]
    fn selecting_moves_between_the_three_and_stops_at_the_ends() {
        let mut d = dial(0, 0, 0);
        assert_eq!(d.channel(), Channel::Hue);
        assert!(!d.select(-1), "nothing left of hue");
        assert!(d.select(1) && d.select(1));
        assert_eq!(d.channel(), Channel::Brightness);
        assert!(!d.select(1), "nothing right of brightness");
        assert_eq!(Channel::ALL.map(Channel::letter), ['H', 'S', 'B']);
    }

    /// Saturation turned down to nothing and up again comes back to the hue it had, not to red:
    /// the dial keeps the hue while it has no effect.
    #[test]
    fn a_hue_survives_its_saturation_going_to_nothing() {
        let mut d = dial(30, 144, 255);
        let hue = d.shown(Channel::Hue);
        d.select(1);
        d.turn(-100);
        assert!(!d.in_effect(Channel::Hue), "no saturation, no hue");
        d.turn(60);
        assert_eq!(d.shown(Channel::Hue), hue);
        assert_ne!(d.rgb().b, 0, "blue again, not red");
    }

    /// Which numbers do anything right now: hue needs saturation and light, saturation light.
    #[test]
    fn a_channel_with_no_effect_says_so() {
        let everything = |d: &Dial| Channel::ALL.map(|c| d.in_effect(c));
        assert_eq!(everything(&dial(30, 144, 255)), [true, true, true]);
        assert_eq!(everything(&dial(128, 128, 128)), [false, true, true], "a grey");
        assert_eq!(everything(&dial(0, 0, 0)), [false, false, true], "black");
    }

    // ---- the strips ----------------------------------------------------------------------------

    /// A saturation or brightness strip runs from its highest at the top to its lowest at the
    /// bottom, holding the other two — every cell a colour one turn could reach.
    #[test]
    fn a_strip_runs_from_highest_to_lowest_holding_the_other_two() {
        let d = dial(255, 0, 0);
        let brightness = d.strip(Channel::Brightness, 11);
        assert_eq!(brightness.len(), 11);
        assert_eq!((brightness[0], brightness[10]), (Rgb::new(255, 0, 0), Rgb::new(0, 0, 0)));
        let saturation = d.strip(Channel::Saturation, 11);
        assert_eq!(saturation[10], Rgb::new(255, 255, 255), "no saturation at full brightness");
    }

    /// The hue strip is the pure hues whatever the colour — the whole wheel from red at the top
    /// round to red at the bottom — so even on a grey it shows where the hue is.
    #[test]
    fn the_hue_strip_is_the_pure_wheel_even_on_a_grey() {
        for d in [dial(255, 0, 0), dial(128, 128, 128), dial(0, 0, 0)] {
            let hue = d.strip(Channel::Hue, 7);
            let red = Rgb::new(255, 0, 0);
            assert_eq!((hue[0], hue[6]), (red, red), "360 at the top is 0 at the bottom");
            assert_eq!(hue[3], Rgb::new(0, 255, 255), "halfway round, cyan");
        }
    }

    /// The marker stands beside the cell nearest the value: the top for the highest, the bottom
    /// for the lowest — and a hue of 0 at the bottom, where counting up starts.
    #[test]
    fn the_marker_is_beside_the_cell_nearest_the_value() {
        let mut d = dial(255, 0, 0);
        assert_eq!(d.marker(Channel::Saturation, 11), 0, "full saturation, the top");
        assert_eq!(d.marker(Channel::Hue, 11), 10, "hue 0, the bottom");
        d.turn(180);
        assert_eq!(d.marker(Channel::Hue, 11), 5, "hue 180, the middle");
        d.turn(-1);
        assert_eq!(d.marker(Channel::Hue, 11), 5, "hue 179, still nearest the middle");
        d.select(2);
        d.turn(-100);
        assert_eq!(d.marker(Channel::Brightness, 11), 10, "no brightness, the bottom");
    }

    /// However short a strip and wherever the value, the marker is on it.
    #[test]
    fn the_marker_is_always_on_its_strip() {
        let mut d = dial(10, 20, 31);
        for step in 0..400 {
            d.turn(1);
            if step % 150 == 149 {
                d.select(1);
            }
            for cells in [1, 2, 3, 11, 40] {
                for channel in Channel::ALL {
                    assert!(d.marker(channel, cells) < cells, "{channel:?} in {cells}");
                }
            }
        }
    }
}
