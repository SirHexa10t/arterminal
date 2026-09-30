//! The crumpled form of a drawing: its document, smaller, and still text a line at a time — for
//! keeping drawings in a repository, where every tool diffs, merges and blames line by line.
//!
//! Text in, text out. [`crumple`] takes a document as [`crate::document`] writes it — or any
//! text at all — and [`uncrumple`] gives it back, so a tool keeping a repository of drawings
//! needs nothing else from this crate: no picker, no parser. [`crate::document::read`] and
//! [`crate::document::write`] use them for any file whose name ends in [`SUFFIX`].
//!
//! # What every version keeps
//!
//! - **Lossless.** `uncrumple(&crumple(text))` is `text`, byte for byte, whatever `text` is —
//!   carriage returns, escapes, a missing final newline and all.
//! - **Line by line.** Each row of a drawing is a line of its crumpled form, or one in each of
//!   its planes, made from that row alone — with, at most, tables the whole drawing shares, as
//!   version 1's character code and its palette. So painting one row changes one line, and a
//!   diff says which. A crumpled line never holds a `\n` of its own.
//! - **Never larger.** A crumpled form is never longer than version 0's, the text as it is: when
//!   a newer version would not be smaller, the text is written as version 0.
//! - **Versioned.** The first line names the version it was crumpled with, and each version
//!   reads every earlier one, so a file crumpled today reads tomorrow. A version this build does
//!   not know is refused by name, never guessed at, and anything a writer of a version it knows
//!   would not have written is refused too, by line — never taken for something it is not, and
//!   never made to build more than a drawing crumpled so could hold.
//!
//! The line-by-line rule is the one that costs. A whole-file compressor gets much further on the
//! same drawings, because the redundancy between one row of art and the next is real — but its
//! output changes all over when one cell does, and a repository of drawings is then a repository
//! of opaque blobs. Here, a diff of a crumpled drawing is still a diff of the drawing.
//!
//! # The versions
//!
//! **Version 0** is the identity: after the header, the text split on `\n` exactly, each line
//! itself — so the newlines alone say whether the text ends in one. It is written for any text
//! that is not a drawing's document as this crate writes it, and for any that version 1 would not
//! make smaller. Lossless absolutely.
//!
//! **Version 1** is a document as two planes, its characters and its colours, each a line a row
//! — see `crumple::planes` for the format. It is written only for a document exactly as
//! [`crate::document::render`] writes it, and decoded through that same writer: so it is lossless
//! for as long as the writer spells documents as it does now. A change to that spelling needs a
//! new version, and a reader of this one kept as it was — which a tripwire in the tests insists on.
//! The palette's order is what its colour numbers count. The picker only ever adds to its end —
//! its one removal is undoing the swatch added last — but [`crate::Palette::remove`] can take one
//! out of the middle, and then every later swatch is numbered one less: files already crumpled
//! stay right, each being whole in itself, but the next crumpled form rewrites every colour line
//! that numbers a later swatch.
//!
//! Its next lever, measured and not yet pulled: the likeness of a row to the row above it, which
//! is most of what a whole-file compressor gets that this does not — taking it would make an edit
//! change two lines rather than one.

use std::borrow::Cow;
use std::fmt;
use std::path::{Path, PathBuf};

mod base64;
mod huffman;
mod planes;

/// Appended to a document's path to name its crumpled form: `art.txt` crumples to
/// `art.txt.crumpled`, keeping the whole of the name it had, as the salvage suffix does.
pub const SUFFIX: &str = ".crumpled";

/// The newest version, which [`crumple`] writes whenever it can — see the module docs for what
/// each version is — and the newest [`uncrumple`] reads.
pub const VERSION: u32 = 1;

/// How the first line of a crumpled form starts and ends, with the version between: in the style
/// of the palette's marker line, so a crumpled file reads as this program's at a glance.
const HEADER_START: &str = "=== arterminal crumpled ";
const HEADER_END: &str = " ===";

/// `text` crumpled — as version 1 when it is a drawing's document as this crate writes it and
/// that is smaller, and as version 0 otherwise. See the module docs for what is promised about
/// the result, whatever `text` is.
pub fn crumple(text: &str) -> String {
    let body: Vec<Cow<'_, str>> = text.split('\n').map(crumple_line).collect();
    let plain = format!("{}\n{}", header(0), body.join("\n"));
    match planes::encode(text) {
        Some(planes) if header(1).len() + 1 + planes.len() < plain.len() => {
            format!("{}\n{planes}", header(1))
        }
        _ => plain,
    }
}

/// The first line of a crumpled form of `version`.
fn header(version: u32) -> String {
    format!("{HEADER_START}{version}{HEADER_END}")
}

/// A crumpled form's text, as it was before it was crumpled — by any version up to [`VERSION`].
pub fn uncrumple(crumpled: &str) -> Result<String, UncrumpleError> {
    let (version, body) = split_header(crumpled)?;
    match version {
        0 => Ok(body.split('\n').collect::<Vec<_>>().join("\n")),
        1 => planes::decode(body),
        _ => unreachable!("split_header refuses every version this build cannot read"),
    }
}

/// Whether `text` is a crumpled form — of any version, including one this build cannot read.
pub fn is_crumpled(text: &str) -> bool {
    !matches!(split_header(text), Err(UncrumpleError::NotCrumpled))
}

/// Whether `path` names a crumpled form: whether [`SUFFIX`] is its extension, as in
/// `art.txt.crumpled`. A file named `.crumpled` and nothing more is not one: it names no document
/// for it to be the crumpled form of.
pub fn is_crumpled_path(path: &Path) -> bool {
    path.extension().is_some_and(|extension| *extension == SUFFIX[1..])
}

/// Where the crumpled form of the document at `path` goes: `path` with [`SUFFIX`] appended — or
/// `path` itself, when it already names one.
pub fn crumpled_path(path: &Path) -> PathBuf {
    if is_crumpled_path(path) {
        return path.to_path_buf();
    }
    let mut name = path.as_os_str().to_owned();
    name.push(SUFFIX);
    PathBuf::from(name)
}

/// The document a crumpled form at `path` is of: `path` with [`SUFFIX`] taken off — or `path`
/// itself, when it names no crumpled form. Where a plain save of a drawing opened from its
/// crumpled form goes.
pub fn plain_path(path: &Path) -> PathBuf {
    match is_crumpled_path(path) {
        true => path.with_extension(""),
        false => path.to_path_buf(),
    }
}

/// One line of a text in version 0: itself.
fn crumple_line(line: &str) -> Cow<'_, str> {
    Cow::Borrowed(line)
}

/// The version a crumpled form's first line names, and everything after that line.
///
/// Tolerant of the two things tools do to a text file's first line without being asked: a
/// byte-order mark before it, which the loaders take off too, and a carriage return after it,
/// which git's line-ending conversion puts there. Either would otherwise make a sound file look
/// like no crumpled form at all.
fn split_header(text: &str) -> Result<(u32, &str), UncrumpleError> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let (first, body) = text.split_once('\n').unwrap_or((text, ""));
    let first = first.strip_suffix('\r').unwrap_or(first);
    let version = first
        .strip_prefix(HEADER_START)
        .and_then(|rest| rest.strip_suffix(HEADER_END))
        .ok_or(UncrumpleError::NotCrumpled)?;
    match version.parse::<u32>() {
        Ok(known) if (0..=VERSION).contains(&known) => Ok((known, body)),
        _ => Err(UncrumpleError::UnknownVersion { version: version.to_string() }),
    }
}

/// Why a text could not be uncrumpled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UncrumpleError {
    /// Its first line is not a crumpled form's header: it is not one at all.
    NotCrumpled,
    /// Its header names a version this build cannot read — crumpled by a newer build, most
    /// likely. The version as the header spells it.
    UnknownVersion { version: String },
    /// A version this build reads, holding what no writer of it would have written: damaged, or
    /// made to do harm. The line, counting the header as 1, and what is wrong with it.
    Malformed { line: usize, why: &'static str },
}

impl fmt::Display for UncrumpleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotCrumpled => write!(
                f,
                "not a crumpled drawing: its first line should be {:?}, or name an earlier version",
                header(VERSION)
            ),
            Self::UnknownVersion { version } => write!(
                f,
                "crumpled by version {version:?}, which this build cannot read — it reads \
                 version {VERSION} and earlier"
            ),
            Self::Malformed { line, why } => {
                write!(f, "a damaged crumpled drawing: line {line}: {why}")
            }
        }
    }
}

impl std::error::Error for UncrumpleError {}

#[cfg(test)]
mod tests {
    use super::*;

    /// A small seeded generator, so a failing text can be reproduced from its seed.
    struct XorShift(u64);

    impl XorShift {
        fn below(&mut self, n: usize) -> usize {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 % n as u64) as usize
        }
    }

    /// Texts that sit exactly where a codec's escaping goes wrong: newlines at either end, none at
    /// all, carriage returns in and out of place, escapes, a byte-order mark, and the `~` a codec
    /// is likely to take for its sigil — alone, doubled, in odd runs, at the end of a line, either
    /// side of a newline.
    const AWKWARD: &[&str] = &[
        "",
        "\n",
        "\n\n",
        "a",
        "a\n",
        "\na",
        "a\r\nb\r\n",
        "a\rb",
        "\r",
        "\u{feff}a\n",
        "\x1b[38;2;255;0;0mab\x1b[0m..\n",
        "=== arterminal crumpled 0 ===\n",
        "=== arterminal palette ===\nsky\t#1e90ff\n",
        "~",
        "~~",
        "~~~",
        "~~~~~",
        "a~\nb",
        "a\n~b",
        "~\n~",
        "~0~1~~2~~~",
        "\u{2800}\u{28ff}\u{2588}\u{2591}",
    ];

    /// Random texts over an alphabet that has a little of everything a document holds, and of
    /// everything that is not supposed to be in one.
    fn random_texts() -> impl Iterator<Item = String> {
        const ALPHABET: &[char] = &[
            'a', '#', ' ', '.', '0', '9', '~', '=', ';', '\n', '\n', '\r', '\x1b', '[', 'm', '\t',
            '\u{e9}', '\u{2800}', '\u{28ff}', '\u{2588}', '\u{feff}', '\u{6f22}',
        ];
        (1..=500u64).map(|seed| {
            let mut rng = XorShift(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
            let len = rng.below(120);
            (0..len).map(|_| ALPHABET[rng.below(ALPHABET.len())]).collect()
        })
    }

    fn every_text() -> impl Iterator<Item = String> {
        AWKWARD.iter().map(|text| text.to_string()).chain(random_texts())
    }

    // ---- lossless --------------------------------------------------------------------------------

    /// Whatever goes in comes back out, byte for byte.
    #[test]
    fn uncrumpling_gives_back_exactly_what_was_crumpled() {
        for text in every_text() {
            assert_eq!(uncrumple(&crumple(&text)).as_deref(), Ok(text.as_str()), "{text:?}");
        }
    }

    /// The drawings that ship with the crate, too — through the real writer, as a save makes them.
    #[test]
    fn the_example_drawings_come_back_exactly() {
        for text in examples() {
            let canonical = crate::document::parse(text)
                .map(|doc| crate::document::render(&doc.canvas, &doc.palette))
                .expect("the examples load");
            for text in [text.to_string(), canonical] {
                assert_eq!(uncrumple(&crumple(&text)), Ok(text));
            }
        }
    }

    fn examples() -> [&'static str; 3] {
        [
            include_str!("../examples/long_ascii_anime_postergirl.txt"),
            include_str!("../examples/small_braille_kirino.txt"),
            include_str!("../examples/small_filled_blocks_miku.txt"),
        ]
    }

    // ---- line by line -------------------------------------------------------------------------

    /// Whether `crumpled` is written as version `version`.
    fn is_version(crumpled: &str, version: u32) -> bool {
        crumpled.split('\n').next() == Some(header(version).as_str())
    }

    /// Version 0 is the header and then one line per line of the text — so no crumpled line holds
    /// a newline of its own. Every text is one version or the other.
    #[test]
    fn version_0_is_its_header_and_then_a_line_per_line() {
        for text in every_text() {
            let crumpled = crumple(&text);
            assert!(is_version(&crumpled, 0) || is_version(&crumpled, 1), "{crumpled:?}");
            if is_version(&crumpled, 0) {
                assert_eq!(crumpled.split('\n').count(), 1 + text.split('\n').count(), "{text:?}");
            }
        }
    }

    /// Whatever the text, its crumpled form is no longer than version 0's — the text as it is,
    /// under the header — so crumpling never makes anything larger.
    #[test]
    fn crumpling_never_makes_a_text_larger() {
        let documents = examples().map(|text| {
            let doc = crate::document::parse(text).expect("the examples load");
            crate::document::render(&doc.canvas, &doc.palette)
        });
        for text in every_text().chain(documents) {
            let plain = header(0).len() + 1 + text.len();
            assert!(crumple(&text).len() <= plain, "{text:?}");
        }
    }

    /// A drawing's document as this crate writes it is crumpled as version 1, and a good deal
    /// smaller for it — pinned, so a change that loses the gain announces itself. The measured
    /// sizes are well inside these; whole-file gzip, which no diff could read, gets 62-89%.
    #[test]
    fn a_drawings_document_is_crumpled_smaller_as_version_1() {
        for (text, at_most) in examples().into_iter().zip([0.35, 0.60, 0.25]) {
            let doc = crate::document::parse(text).expect("the examples load");
            let written = crate::document::render(&doc.canvas, &doc.palette);
            let crumpled = crumple(&written);
            assert!(is_version(&crumpled, 1));
            let ratio = crumpled.len() as f64 / written.len() as f64;
            assert!(ratio <= at_most, "{ratio:.2} of the document, where {at_most} was allowed");
        }
    }

    /// In version 0, change one line of a text, and exactly one line of its crumpled form changes
    /// — the one standing for it, after the header. (Version 1's rows are held to the same by
    /// `crumple::planes`' tests, painting and all.)
    #[test]
    fn in_version_0_one_changed_line_is_one_changed_crumpled_line() {
        let plain = |text: &String| is_version(&crumple(text), 0);
        for (seed, text) in random_texts().enumerate().filter(|(_, text)| plain(text)) {
            let mut lines: Vec<String> = text.split('\n').map(str::to_string).collect();
            let at = seed % lines.len();
            let before = crumple(&text);
            lines[at].push_str("~x");
            let changed_text = lines.join("\n");
            if !plain(&changed_text) {
                continue;
            }
            let after = crumple(&changed_text);
            let changed: Vec<usize> = before
                .split('\n')
                .zip(after.split('\n'))
                .enumerate()
                .filter(|(_, (was, now))| was != now)
                .map(|(line, _)| line)
                .collect();
            assert_eq!(changed, [at + 1], "text {seed}, line {at}");
        }
    }

    // ---- the writer version 1 decodes through ---------------------------------------------------

    /// THE TRIPWIRE. Version 1 decodes through `document::render`, so what it gives back is spelled
    /// by the writer of the day — and were that spelling to change, every version 1 file already
    /// written would read back as other bytes than went in, with nothing to say so. So the writer's
    /// spelling of every kind of colour is pinned here: a colour of its own, a slot of each range,
    /// a reset, and a derived swatch.
    ///
    /// If this fails because the writer was changed on purpose: bump [`VERSION`], and keep for
    /// version 1 a reader that spells documents as the writer did — a frozen copy of it — OR
    /// record here why every version 1 file already written still decodes to the same bytes.
    /// Changing the pinned text without one or the other is how a tripwire stops being one.
    ///
    /// Recorded so far:
    /// - When rows came to keep coloured trailing spaces and the longest row the drawing's width,
    ///   no version 1 file had been committed; and any written before decodes the same, since
    ///   the writer then left no coloured trailing space for a colour line to describe, and its
    ///   longest row already set the width it is padded to now.
    #[test]
    fn the_writers_spelling_that_version_1_decodes_through_is_pinned() {
        use crate::{Canvas, HsbOffset, Ink, Palette, Rgb};
        let mut palette = Palette::new();
        palette.push("ember", Rgb::new(226, 57, 57)).expect("fresh");
        palette.push("theme", Ink::Slot(3)).expect("fresh");
        palette.push("bright", Ink::Slot(9)).expect("fresh");
        palette.push("cube", Ink::Slot(196)).expect("fresh");
        let darker = HsbOffset { hue: 0, saturation: 0, brightness: -60 };
        palette.push_derived("shade", "ember", darker).expect("ember is there to follow");
        let mut canvas = Canvas::from_text("ab cd\ne").expect("valid");
        let inks = [
            ((0, 0), "ember"),
            ((1, 0), "theme"),
            ((3, 0), "bright"),
            ((4, 0), "cube"),
            ((0, 1), "shade"),
        ];
        for ((x, y), label) in inks {
            canvas.cell_mut(x, y).expect("inside").ink = palette.get(label).map(|s| s.color());
        }
        let pinned =
            "\x1b[38;2;226;57;57ma\x1b[38;5;3mb\x1b[0m \x1b[38;5;9mc\x1b[38;5;196md\x1b[0m\n\
                      \x1b[38;2;166;42;42me\x1b[0m\n\
                      === arterminal palette ===\n\
                      ember\t#e23939\ntheme\tslot 3\nbright\tslot 9\ncube\tslot 196\n\
                      shade\t#a62a2a\tember\t0,0,-60\n";
        assert_eq!(
            crate::document::render(&canvas, &palette),
            pinned,
            "document::render spells documents differently now, and crumpled version 1 decodes \
             through it: bump crumple::VERSION and keep a version 1 reader that spells them as before"
        );
        // Through version 1 itself: this drawing is too small to be crumpled as one.
        let body = planes::encode(pinned).expect("a document as the writer writes it");
        assert_eq!(planes::decode(&body).as_deref(), Ok(pinned));

        // And what of a row is written: a coloured space at its end is kept, and the longest row
        // carries the drawing's width out in spaces.
        let mut palette = Palette::new();
        palette.push("sea", Rgb::new(30, 144, 255)).expect("fresh");
        let mut canvas = Canvas::from_text("gh   \ni").expect("valid");
        canvas.cell_mut(2, 0).expect("inside").ink = palette.get("sea").map(|s| s.color());
        let pinned =
            "gh\x1b[38;2;30;144;255m \x1b[0m  \ni\n=== arterminal palette ===\nsea\t#1e90ff\n";
        assert_eq!(
            crate::document::render(&canvas, &palette),
            pinned,
            "document::render writes rows differently now, and crumpled version 1 decodes \
             through it: bump crumple::VERSION and keep a version 1 reader that writes them as before"
        );
        let body = planes::encode(pinned).expect("a document as the writer writes it");
        assert_eq!(planes::decode(&body).as_deref(), Ok(pinned));
    }

    // ---- versions and headers --------------------------------------------------------------------

    /// A version this build cannot read is refused by name, and never read as a known one.
    #[test]
    fn an_unknown_version_is_refused_by_name() {
        let next = (VERSION + 1).to_string();
        for version in [next.as_str(), "99", "0.5", "x", ""] {
            let text = format!("{HEADER_START}{version}{HEADER_END}\nab\n");
            let refused = uncrumple(&text).unwrap_err();
            assert_eq!(refused, UncrumpleError::UnknownVersion { version: version.to_string() });
            assert!(refused.to_string().contains(&format!("{version:?}")), "{refused}");
            assert!(is_crumpled(&text), "crumpled all the same, by something newer");
        }
    }

    /// A text with no crumpled header is not one, and says what the header should have been.
    #[test]
    fn a_text_without_the_header_is_not_crumpled() {
        for text in ["", "ab\n", "=== arterminal palette ===\n", " === arterminal crumpled 0 ===\n"]
        {
            assert_eq!(uncrumple(text), Err(UncrumpleError::NotCrumpled), "{text:?}");
            assert!(!is_crumpled(text));
        }
        let why = UncrumpleError::NotCrumpled.to_string();
        assert!(why.contains(&header(VERSION)), "{why}");
    }

    /// What tools do to a first line unasked — a byte-order mark before it, a carriage return
    /// after it — does not stop a crumpled form reading as one.
    #[test]
    fn a_byte_order_mark_or_a_carriage_return_does_not_hide_the_header() {
        let crumpled = crumple("ab\ncd");
        let (header, body) = crumpled.split_once('\n').expect("a header line");
        for touched in [format!("\u{feff}{crumpled}"), format!("{header}\r\n{body}")] {
            assert_eq!(uncrumple(&touched), Ok("ab\ncd".to_string()), "{touched:?}");
        }
        assert_eq!(uncrumple(header), Ok(String::new()), "a header alone is the empty text");
    }

    // ---- paths ---------------------------------------------------------------------------------

    /// A crumpled form's name is the document's with the suffix added, whole, and the document's
    /// is the crumpled form's with it taken off — each once, and each leaving a name that is
    /// already so as it is.
    #[test]
    fn a_crumpled_form_is_named_by_adding_the_suffix_and_its_document_by_taking_it_off() {
        let plain = Path::new("art/sky.txt");
        let crumpled = crumpled_path(plain);
        assert_eq!(crumpled, Path::new("art/sky.txt.crumpled"));
        assert!(!is_crumpled_path(plain) && is_crumpled_path(&crumpled));
        assert_eq!(crumpled_path(&crumpled), crumpled, "added once, not twice");
        assert_eq!(plain_path(&crumpled), plain);
        assert_eq!(plain_path(plain), plain, "taken off only what is there");
        assert_eq!(plain_path(Path::new("sky.crumpled")), Path::new("sky"));
        assert!(!is_crumpled_path(Path::new("sky.crumpled.txt")), "the suffix is at the end");
        assert!(!is_crumpled_path(Path::new(".crumpled")), "a name of nothing but the suffix");
    }
}
