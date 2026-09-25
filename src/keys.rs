//! Turning the bytes a terminal sends into key events — presses, repeats, and releases.
//!
//! # Why this exists instead of `console::Term::read_key`
//!
//! Two reasons, and either alone would have been enough.
//!
//! **Releases.** A classic terminal reports only key PRESSES: holding Space auto-repeats Space,
//! and letting go of it sends nothing at all. "Stop painting when Space is released" is therefore
//! impossible to hear — unless the terminal speaks the kitty keyboard protocol, which reports
//! press, repeat and release as separate events. `console` does not know that protocol exists.
//!
//! **Function keys, deterministically.** `console`'s reader (`read_single_key_impl` in its
//! `unix_term.rs`) takes an `ESC` plus AT MOST three more bytes — there is no loop to a CSI final
//! byte — and has no branch for `SS3` at all. So F5 (`ESC [ 1 5 ~`) comes back as an unknown
//! escape with its `~` left in the buffer to surface as a stray typed key, and F1–F4 (`ESC O P`…)
//! come back as an unknown escape plus a stray letter. Every time, with every byte already
//! buffered: this is not a race a longer timeout would fix. On top of that it reads each
//! continuation byte with a zero-timeout poll, so an arrow whose bytes straddle two reads becomes
//! a bare Escape — which in this program is a request to close.
//!
//! [`Decoder`] scans to the real final byte and keeps a partial sequence until the rest arrives.
//! Only a LONE `ESC` needs a clock to settle, because only that one is a complete key and a
//! prefix at once; everything else partial is provably unfinished. See
//! [`Decoder::waiting_on_lone_escape`].
//!
//! # Ctrl+C is a key here, not a signal
//!
//! `console`'s plain `read_key` answers Ctrl+C by raising SIGINT on the process, and the default
//! handler ends it before raw mode can be undone — the shell is left raw with its cursor hidden.
//! Raw mode turns signal generation off, so the byte `0x03` simply arrives, and this decoder hands
//! it over as Ctrl+C like any other chord. That is the whole reason the picker never has to handle
//! SIGINT: nothing ever raises it.
//!
//! # What is understood
//!
//! Legacy encodings: printable UTF-8, the C0 controls (Ctrl+letter, Enter, Tab, Backspace),
//! `ESC`-prefixed Alt, `CSI … ~` and `CSI 1 ; m X` functional keys, `SS3` arrows and F1–F4, and
//! the Linux console's `ESC [ [ A`–`E` for F1–F5. Kitty's `CSI code ; mods:event ; text u`, and
//! its event-type sub-field on the legacy forms. And two terminal REPLIES, which arrive on the
//! same stream as keys: the keyboard-flags answer `CSI ? flags u` and the primary device
//! attributes `CSI ? … c`, used together to detect whether the protocol is spoken at all.
//!
//! Pure: bytes in, events out, no terminal anywhere — so every encoding below is a test.

/// Which key, with modifiers stripped off into [`Mods`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KeyCode {
    /// A key that types text — or, with Ctrl held, a Ctrl+letter chord: `Char('s')` + ctrl is
    /// Ctrl+S whether it arrived as the legacy byte `0x13` or as kitty's `CSI 115;5u`.
    Char(char),
    Enter,
    Tab,
    /// Shift+Tab. Terminals send it as its own sequence, so it is its own key.
    BackTab,
    Backspace,
    Delete,
    Insert,
    Escape,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    /// Function key `n`, from 1.
    F(u8),
    /// Shift, Ctrl, Alt and the rest, pressed or released on their own. Kitty reports these once
    /// every key is an escape; nothing here acts on them, and they must never interrupt a hold.
    Modifier,
    /// Well-formed, but not a key this program has a use for.
    Unknown,
}

/// What happened to the key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum KeyKind {
    /// Went down — and, on a terminal without event types, also every auto-repeat, since the two
    /// cannot be told apart there.
    #[default]
    Press,
    /// Still held; the terminal's auto-repeat. Only reported under the kitty protocol.
    Repeat,
    /// Let go. Only reported under the kitty protocol.
    Release,
}

/// The modifiers this program distinguishes. Super, Hyper, Meta and the lock keys are read and
/// dropped: Caps Lock being on must not turn Space into a different key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Mods {
    pub shift: bool,
    pub alt: bool,
    pub ctrl: bool,
}

/// One key event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct KeyEvent {
    /// Which key — what a binding matches. For a letter under the kitty protocol this is the
    /// BASE key: Shift+A is `Char('a')` with shift, so a binding can name the key it means.
    pub code: KeyCode,
    pub kind: KeyKind,
    pub mods: Mods,
    /// What the key TYPED, if it typed anything — what a text field reads. `None` for chords,
    /// releases and functional keys. Kept apart from `code` because the two diverge exactly where
    /// it matters: Shift+A types `A`, a Mac's Alt+A types `å`, and neither is the key's name.
    pub text: Option<char>,
}

impl KeyEvent {
    /// A press with no modifiers. A character key's press types that character.
    pub const fn press(code: KeyCode) -> Self {
        let text = match code {
            KeyCode::Char(c) => Some(c),
            _ => None,
        };
        Self {
            code,
            kind: KeyKind::Press,
            mods: Mods { shift: false, alt: false, ctrl: false },
            text,
        }
    }

    /// An auto-repeat, which types again just as a press does.
    pub const fn repeat(code: KeyCode) -> Self {
        Self { kind: KeyKind::Repeat, ..Self::press(code) }
    }

    /// A release, which never types.
    pub const fn release(code: KeyCode) -> Self {
        Self { kind: KeyKind::Release, text: None, ..Self::press(code) }
    }

    /// Ctrl plus a letter, pressed — `KeyEvent::ctrl('s')` is Ctrl+S. A chord types nothing.
    pub const fn ctrl(letter: char) -> Self {
        Self {
            mods: Mods { shift: false, alt: false, ctrl: true },
            text: None,
            ..Self::press(KeyCode::Char(letter))
        }
    }

    /// Whether this is `letter` with Ctrl held and nothing else, in any event kind.
    pub fn is_ctrl(&self, letter: char) -> bool {
        self.code == KeyCode::Char(letter) && self.mods.ctrl && !self.mods.alt
    }
}

/// A bare key code is its press with no modifiers — so a caller can write `KeyCode::Enter`
/// wherever a [`KeyEvent`] is wanted.
impl From<KeyCode> for KeyEvent {
    fn from(code: KeyCode) -> Self {
        Self::press(code)
    }
}

/// One unit of what a terminal sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decoded {
    Key(KeyEvent),
    /// `CSI ? flags u`: the terminal speaks the kitty keyboard protocol, currently with `flags`.
    KeyboardFlags(u32),
    /// `CSI ? … c`: the primary device attributes. Every terminal answers this one, which is what
    /// makes it the end marker for the protocol query.
    DeviceAttributes,
}

/// A parser that remembers an incomplete sequence between reads.
#[derive(Debug, Default)]
pub struct Decoder {
    pending: Vec<u8>,
}

/// Longest escape sequence worth waiting for. Anything longer without a final byte is garbage —
/// a paste of binary, a terminal bug — and is dropped rather than held for ever.
const MAX_SEQUENCE: usize = 64;

impl Decoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Decode as much as `bytes` completes. An unfinished sequence at the end is kept for the next
    /// call — or for [`Decoder::flush`], if the caller decides nothing more is coming.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Decoded> {
        self.pending.extend_from_slice(bytes);
        let mut out = Vec::new();
        let mut at = 0;
        while at < self.pending.len() {
            match parse(&self.pending[at..]) {
                Step::Done(len, decoded) => {
                    out.extend(decoded);
                    at += len;
                }
                Step::Partial if self.pending.len() - at <= MAX_SEQUENCE => break,
                // Too long to be a real sequence: drop the introducer and resynchronise.
                Step::Partial => at += 1,
            }
        }
        self.pending.drain(..at);
        out
    }

    /// Whether a sequence is waiting on bytes that have not arrived.
    /// Whether what is held is exactly one `ESC` — the only partial that is also a complete key.
    ///
    /// The caller should give this one a SHORT wait before [`Decoder::flush`]: it may be the
    /// Escape key, and a person notices a sticky Escape. Anything else partial — an `ESC [` or
    /// `ESC O` introducer held, a UTF-8 character split across reads — is provably unfinished, no
    /// valid input ends there, so it deserves a LONG wait: cutting it short is exactly the tear a
    /// slow link produces, and what this decoder exists to prevent.
    pub fn waiting_on_lone_escape(&self) -> bool {
        self.pending == [0x1b]
    }

    pub fn has_partial(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Nothing more is coming: settle what is held. A lone `ESC` was the Escape key; anything else
    /// half-finished was a sequence cut short, and is dropped rather than typed.
    pub fn flush(&mut self) -> Vec<Decoded> {
        match std::mem::take(&mut self.pending).as_slice() {
            [0x1b] => vec![Decoded::Key(KeyEvent::press(KeyCode::Escape))],
            _ => Vec::new(),
        }
    }
}

/// How one parse attempt went.
enum Step {
    /// Consumed this many bytes, producing zero or more units.
    Done(usize, Option<Decoded>),
    /// The bytes so far are the start of something longer.
    Partial,
}

/// An event that types nothing.
fn key(code: KeyCode, mods: Mods, kind: KeyKind) -> Option<Decoded> {
    Some(Decoded::Key(KeyEvent { code, kind, mods, text: None }))
}

fn parse(bytes: &[u8]) -> Step {
    match bytes {
        [] => Step::Partial,
        [0x1b] => Step::Partial,
        [0x1b, b'[', ..] => parse_csi(bytes),
        [0x1b, b'O', ..] => parse_ss3(bytes),
        // Escape pressed twice: the first is a key of its own.
        [0x1b, 0x1b, ..] => Step::Done(1, key(KeyCode::Escape, Mods::default(), KeyKind::Press)),
        // `ESC` before anything else is Alt held on that key.
        [0x1b, rest @ ..] => match parse(rest) {
            Step::Done(len, Some(Decoded::Key(mut event))) => {
                event.mods.alt = true;
                event.text = None;
                Step::Done(len + 1, Some(Decoded::Key(event)))
            }
            Step::Done(len, other) => Step::Done(len + 1, other),
            Step::Partial => Step::Partial,
        },
        [byte, ..] if *byte < 0x20 || *byte == 0x7f => Step::Done(1, c0(*byte)),
        _ => parse_utf8(bytes),
    }
}

/// A C0 control byte, as the key a person pressed to make it.
fn c0(byte: u8) -> Option<Decoded> {
    let ctrl = Mods { ctrl: true, ..Mods::default() };
    let (code, mods) = match byte {
        b'\r' | b'\n' => (KeyCode::Enter, Mods::default()),
        b'\t' => (KeyCode::Tab, Mods::default()),
        0x7f | 0x08 => (KeyCode::Backspace, Mods::default()),
        0x00 => (KeyCode::Char(' '), ctrl),
        0x01..=0x1a => (KeyCode::Char((b'a' + byte - 1) as char), ctrl),
        0x1c..=0x1f => (KeyCode::Char(['\\', ']', '^', '_'][(byte - 0x1c) as usize]), ctrl),
        _ => (KeyCode::Unknown, Mods::default()),
    };
    key(code, mods, KeyKind::Press)
}

fn parse_utf8(bytes: &[u8]) -> Step {
    let len = match bytes[0] {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf7 => 4,
        // A continuation byte with no lead: noise. Skip it.
        _ => return Step::Done(1, None),
    };
    if bytes.len() < len {
        return Step::Partial;
    }
    match std::str::from_utf8(&bytes[..len]).ok().and_then(|s| s.chars().next()) {
        Some(c) => Step::Done(len, Some(Decoded::Key(KeyEvent::press(KeyCode::Char(c))))),
        None => Step::Done(1, None),
    }
}

/// `ESC O` and one letter: arrows and F1–F4 in the terminal's application mode.
fn parse_ss3(bytes: &[u8]) -> Step {
    let Some(&letter) = bytes.get(2) else { return Step::Partial };
    let code = letter_key(letter).unwrap_or(KeyCode::Unknown);
    Step::Done(3, key(code, Mods::default(), KeyKind::Press))
}

/// The key a CSI or SS3 final letter names, if any.
fn letter_key(letter: u8) -> Option<KeyCode> {
    Some(match letter {
        b'A' => KeyCode::Up,
        b'B' => KeyCode::Down,
        b'C' => KeyCode::Right,
        b'D' => KeyCode::Left,
        b'H' => KeyCode::Home,
        b'F' => KeyCode::End,
        b'P' => KeyCode::F(1),
        b'Q' => KeyCode::F(2),
        b'R' => KeyCode::F(3),
        b'S' => KeyCode::F(4),
        _ => return None,
    })
}

/// `ESC [` … final byte.
fn parse_csi(bytes: &[u8]) -> Step {
    // The Linux console's F1–F5: `ESC [ [ A` … `ESC [ [ E`. `[` is itself a legal final byte,
    // so this has to be recognised before the general scan would end the sequence on it.
    if bytes.get(2) == Some(&b'[') {
        let Some(&letter) = bytes.get(3) else { return Step::Partial };
        let code = match letter {
            b'A'..=b'E' => KeyCode::F(letter - b'A' + 1),
            _ => KeyCode::Unknown,
        };
        return Step::Done(4, key(code, Mods::default(), KeyKind::Press));
    }
    let Some(offset) = bytes[2..].iter().position(|b| (0x40..=0x7e).contains(b)) else {
        return Step::Partial;
    };
    let end = 2 + offset;
    let body = std::str::from_utf8(&bytes[2..end]).unwrap_or("");
    Step::Done(end + 1, csi(body, bytes[end]))
}

/// A parsed CSI: its private marker, if any, then parameters split on `;` and sub-parameters on
/// `:`. Empty fields stay empty, so "absent" and "zero" remain distinguishable.
fn csi(body: &str, final_byte: u8) -> Option<Decoded> {
    let (private, params) = match body.as_bytes().first() {
        Some(b'?' | b'>' | b'<' | b'=') => (body.as_bytes()[0], &body[1..]),
        _ => (0, body),
    };
    let fields: Vec<Vec<&str>> = params.split(';').map(|p| p.split(':').collect()).collect();
    let number = |field: usize, sub: usize| -> Option<u32> {
        fields.get(field).and_then(|f| f.get(sub)).and_then(|s| s.parse().ok())
    };

    match (private, final_byte) {
        (b'?', b'u') => return Some(Decoded::KeyboardFlags(number(0, 0).unwrap_or(0))),
        (b'?', b'c') => return Some(Decoded::DeviceAttributes),
        (0, _) => {}
        _ => return None, // some other private reply this program never asked for
    }

    let (mods, kind) = modifiers(number(1, 0), number(1, 1));
    let code = match final_byte {
        b'u' => return kitty_key(number(0, 0).unwrap_or(0), fields.get(2), mods, kind),
        b'~' => tilde_key(number(0, 0).unwrap_or(0)),
        b'Z' => KeyCode::BackTab,
        letter => letter_key(letter).unwrap_or(KeyCode::Unknown),
    };
    key(code, mods, kind)
}

/// The modifier field `1 + bits`, with its optional event-type sub-field.
fn modifiers(value: Option<u32>, event: Option<u32>) -> (Mods, KeyKind) {
    let bits = value.unwrap_or(1).saturating_sub(1);
    let mods = Mods { shift: bits & 0b1 != 0, alt: bits & 0b10 != 0, ctrl: bits & 0b100 != 0 };
    let kind = match event {
        Some(2) => KeyKind::Repeat,
        Some(3) => KeyKind::Release,
        _ => KeyKind::Press,
    };
    (mods, kind)
}

/// `CSI number ~`.
fn tilde_key(number: u32) -> KeyCode {
    match number {
        2 => KeyCode::Insert,
        3 => KeyCode::Delete,
        5 => KeyCode::PageUp,
        6 => KeyCode::PageDown,
        1 | 7 => KeyCode::Home,
        4 | 8 => KeyCode::End,
        11..=15 => KeyCode::F((number - 10) as u8),
        17..=21 => KeyCode::F((number - 11) as u8),
        23 | 24 => KeyCode::F((number - 12) as u8),
        _ => KeyCode::Unknown,
    }
}

/// Kitty's `CSI code ; mods:event ; text u`.
///
/// `code` is the key's BASE form — `a` for Shift+A — and becomes [`KeyEvent::code`]. The text the
/// key produced, which flag 16 asks for, becomes [`KeyEvent::text`]; it is what the keyboard
/// layout and Shift actually made. Only when a terminal sends no text for a plain press is it
/// reconstructed from the key, uppercased by hand when Shift is the only reason it would differ.
/// Chords (Ctrl or Alt held) and releases never type.
fn kitty_key(code: u32, text: Option<&Vec<&str>>, mods: Mods, kind: KeyKind) -> Option<Decoded> {
    let keycode = match code {
        27 => KeyCode::Escape,
        13 | 57414 => KeyCode::Enter, // 57414 is the keypad's Enter
        9 if mods.shift => KeyCode::BackTab,
        9 => KeyCode::Tab,
        127 | 8 => KeyCode::Backspace,
        57376..=57398 => KeyCode::F((code - 57376 + 13) as u8),
        57441..=57454 | 57358..=57360 => KeyCode::Modifier, // shifts, ctrls… and the three locks
        57344..=63743 => KeyCode::Unknown, // the rest of the private-use functional keys
        _ => char::from_u32(code).filter(|_| code != 0).map_or(KeyCode::Unknown, KeyCode::Char),
    };
    let sent = text
        .and_then(|points| points.first())
        .and_then(|point| point.parse::<u32>().ok())
        .and_then(char::from_u32);
    let chord = mods.ctrl || mods.alt;
    let typed = match (kind, keycode) {
        (KeyKind::Release, _) => None,
        _ if sent.is_some() => sent,
        (_, KeyCode::Char(c)) if !chord && mods.shift => Some(c.to_ascii_uppercase()),
        (_, KeyCode::Char(c)) if !chord => Some(c),
        _ => None,
    };
    // Text with no key behind it (`CSI 0 ; ; 229 u`): the text IS the key.
    let keycode = match (keycode, typed) {
        (KeyCode::Unknown, Some(c)) if code == 0 => KeyCode::Char(c),
        (other, _) => other,
    };
    Some(Decoded::Key(KeyEvent { code: keycode, kind, mods, text: typed }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(bytes: &[u8]) -> Vec<KeyEvent> {
        let mut decoder = Decoder::new();
        let mut out = decoder.feed(bytes);
        out.extend(decoder.flush());
        out.into_iter()
            .filter_map(|d| match d {
                Decoded::Key(k) => Some(k),
                _ => None,
            })
            .collect()
    }

    fn one(bytes: &[u8]) -> KeyEvent {
        let all = keys(bytes);
        assert_eq!(all.len(), 1, "{bytes:?} gave {all:?}");
        all[0]
    }

    const SHIFT: Mods = Mods { shift: true, alt: false, ctrl: false };
    const CTRL: Mods = Mods { shift: false, alt: false, ctrl: true };

    // ---- legacy ---------------------------------------------------------------------------

    #[test]
    fn printable_text_is_one_key_per_character_multi_byte_included() {
        assert_eq!(
            keys(b"ab").iter().map(|k| k.code).collect::<Vec<_>>(),
            [KeyCode::Char('a'), KeyCode::Char('b')]
        );
        assert_eq!(one("é".as_bytes()).code, KeyCode::Char('é'));
        assert_eq!(one("⣿".as_bytes()).code, KeyCode::Char('⣿'));
        assert_eq!(one(b" ").code, KeyCode::Char(' '));
    }

    #[test]
    fn the_c0_controls_are_the_keys_that_make_them() {
        assert_eq!(one(b"\r").code, KeyCode::Enter);
        assert_eq!(one(b"\t").code, KeyCode::Tab);
        assert_eq!(one(b"\x7f").code, KeyCode::Backspace);
        assert_eq!(one(b"\x13"), KeyEvent::ctrl('s'));
        assert_eq!(one(b"\x1a"), KeyEvent::ctrl('z'));
        assert_eq!(one(b"\x03"), KeyEvent::ctrl('c'));
        assert_eq!(one(b"\x00").mods, CTRL, "ctrl+space");
    }

    #[test]
    fn arrows_and_the_editing_keys_in_every_legacy_spelling() {
        for (bytes, code) in [
            (&b"\x1b[A"[..], KeyCode::Up),
            (b"\x1bOB", KeyCode::Down),
            (b"\x1b[C", KeyCode::Right),
            (b"\x1b[D", KeyCode::Left),
            (b"\x1b[H", KeyCode::Home),
            (b"\x1b[4~", KeyCode::End),
            (b"\x1b[3~", KeyCode::Delete),
            (b"\x1b[2~", KeyCode::Insert),
            (b"\x1b[5~", KeyCode::PageUp),
            (b"\x1b[Z", KeyCode::BackTab),
        ] {
            assert_eq!(one(bytes).code, code, "{bytes:?}");
        }
    }

    /// Every F-key spelling a real terminal sends — including the three for F2 that `console`
    /// split in two, and F5/F6, which it split the same way.
    #[test]
    fn function_keys_in_every_spelling_arrive_whole() {
        for (bytes, n) in [
            (&b"\x1bOP"[..], 1),
            (b"\x1bOQ", 2),
            (b"\x1b[12~", 2),
            (b"\x1b[[B", 2),
            (b"\x1b[[E", 5),
            (b"\x1b[15~", 5),
            (b"\x1b[17~", 6),
            (b"\x1b[24~", 12),
            (b"\x1b[1;1Q", 2),
        ] {
            assert_eq!(one(bytes).code, KeyCode::F(n), "{bytes:?}");
        }
    }

    #[test]
    fn modifiers_on_legacy_sequences_are_read() {
        let shifted = one(b"\x1b[1;2C");
        assert_eq!((shifted.code, shifted.mods), (KeyCode::Right, SHIFT));
        assert_eq!(one(b"\x1b[3;5~").mods, CTRL);
        let alt = one(b"\x1bx");
        assert_eq!((alt.code, alt.mods.alt), (KeyCode::Char('x'), true), "ESC before a key is Alt");
    }

    // ---- the reason the decoder holds partials ----------------------------------------------

    /// A sequence split across reads is reassembled, not turned into a stray Escape plus typing.
    #[test]
    fn a_sequence_split_across_reads_is_kept_until_it_completes() {
        let mut decoder = Decoder::new();
        assert!(decoder.feed(b"\x1b").is_empty());
        assert!(decoder.has_partial());
        assert!(decoder.feed(b"[1").is_empty());
        assert_eq!(decoder.feed(b"5~"), [Decoded::Key(KeyEvent::press(KeyCode::F(5)))]);
        assert!(!decoder.has_partial());

        let mut split_utf8 = Decoder::new();
        assert!(split_utf8.feed(&"⣿".as_bytes()[..1]).is_empty());
        assert_eq!(split_utf8.feed(&"⣿".as_bytes()[1..]).len(), 1);
    }

    /// Only the caller's timeout can say a lone ESC was the Escape key.
    #[test]
    fn a_lone_escape_is_escape_only_once_nothing_follows() {
        let mut decoder = Decoder::new();
        assert!(decoder.feed(b"\x1b").is_empty(), "could still be the start of an arrow");
        assert_eq!(decoder.flush(), [Decoded::Key(KeyEvent::press(KeyCode::Escape))]);
        assert_eq!(keys(b"\x1b\x1b"), [KeyEvent::press(KeyCode::Escape); 2], "twice, both keys");
    }

    #[test]
    fn a_cut_short_sequence_is_dropped_rather_than_typed() {
        let mut decoder = Decoder::new();
        decoder.feed(b"\x1b[1");
        assert_eq!(decoder.flush(), [], "no stray '[' or '1' typed into a name");
    }

    #[test]
    fn a_runaway_sequence_is_dropped_and_decoding_resumes() {
        let mut junk = b"\x1b[".to_vec();
        junk.extend(std::iter::repeat_n(b'1', MAX_SEQUENCE + 5));
        let mut decoder = Decoder::new();
        decoder.feed(&junk);
        let after = decoder.feed(b"\x1b[A");
        assert!(after.contains(&Decoded::Key(KeyEvent::press(KeyCode::Up))), "{after:?}");
    }

    /// Held arrows arrive back to back in one read; every one is a key.
    #[test]
    fn a_burst_of_repeated_arrows_is_every_arrow() {
        assert_eq!(keys(&b"\x1b[C".repeat(40)).len(), 40);
    }

    // ---- kitty ------------------------------------------------------------------------------

    #[test]
    fn kitty_press_repeat_and_release_are_distinguished() {
        assert_eq!(one(b"\x1b[32u"), KeyEvent::press(KeyCode::Char(' ')));
        assert_eq!(one(b"\x1b[32;1:1u"), KeyEvent::press(KeyCode::Char(' ')));
        assert_eq!(one(b"\x1b[32;1:2u"), KeyEvent::repeat(KeyCode::Char(' ')));
        assert_eq!(one(b"\x1b[32;1:3u"), KeyEvent::release(KeyCode::Char(' ')));
        assert_eq!(one(b"\x1b[3;1:3~"), KeyEvent::release(KeyCode::Delete));
        assert_eq!(one(b"\x1b[1;1:3C"), KeyEvent::release(KeyCode::Right));
        assert_eq!(one(b"\x1b[13;1:3u"), KeyEvent::release(KeyCode::Enter));
        assert_eq!(one(b"\x1b[127;1:3u"), KeyEvent::release(KeyCode::Backspace));
    }

    /// With every key an escape, Ctrl+S is `CSI 115;5u` — and must mean what `0x13` means.
    #[test]
    fn kitty_ctrl_chords_match_their_legacy_bytes() {
        assert_eq!(one(b"\x1b[115;5u"), one(b"\x13"));
        assert_eq!(one(b"\x1b[99;5u"), KeyEvent::ctrl('c'));
        assert!(one(b"\x1b[122;5:2u").is_ctrl('z'), "a held Ctrl+Z is a Ctrl+Z repeat");
    }

    #[test]
    fn kitty_associated_text_is_the_character_typed() {
        let shifted = one(b"\x1b[97;2;65u");
        assert_eq!(shifted.code, KeyCode::Char('a'), "the KEY is the base key, for bindings");
        assert_eq!(shifted.text, Some('A'), "the TEXT is what it typed, for fields");
        assert_eq!(one(b"\x1b[97;2u").text, Some('A'), "shift+a with no text sent");
        assert_eq!(one(b"\x1b[0;;229u").text, Some('å'), "text with no key");
    }

    #[test]
    fn kitty_modifier_keys_and_locks_are_their_own_inert_events() {
        assert_eq!(one(b"\x1b[57441u").code, KeyCode::Modifier, "left shift");
        assert_eq!(one(b"\x1b[57442;5:3u").code, KeyCode::Modifier, "left ctrl, released");
        assert_eq!(one(b"\x1b[57358u").code, KeyCode::Modifier, "caps lock");
        assert_eq!(one(b"\x1b[32;65u").mods, Mods::default(), "caps lock on does not modify Space");
    }

    #[test]
    fn the_protocol_query_replies_are_recognised_and_are_not_keys() {
        let mut decoder = Decoder::new();
        let out = decoder.feed(b"\x1b[?1u\x1b[?62;22c");
        assert_eq!(out, [Decoded::KeyboardFlags(1), Decoded::DeviceAttributes]);
        assert_eq!(Decoder::new().feed(b"\x1b[?64;1;2c"), [Decoded::DeviceAttributes]);
    }
}

#[cfg(test)]
mod text_tests {
    use super::*;

    fn one(bytes: &[u8]) -> KeyEvent {
        let mut decoder = Decoder::new();
        let mut out = decoder.feed(bytes);
        out.extend(decoder.flush());
        match out.as_slice() {
            [Decoded::Key(k)] => *k,
            other => panic!("{bytes:?} gave {other:?}"),
        }
    }

    /// The separation, stated as the contract: bindings read `code`, fields read `text`, and a
    /// chord or a release never types.
    #[test]
    fn what_a_key_is_and_what_it_typed_are_kept_apart() {
        assert_eq!(one(b"a").text, Some('a'), "legacy text types itself");
        assert_eq!(one(b"\x13").text, None, "legacy ctrl+s types nothing");
        assert_eq!(one(b"\x1bx").text, None, "alt+x is a chord");
        assert_eq!(one(b"\x1b[115;5u").text, None, "kitty ctrl+s types nothing");
        assert_eq!(one(b"\x1b[97;1:3u").text, None, "a release types nothing");
        assert_eq!(one(b"\x1b[97;1:2u").text, Some('a'), "a repeat types again");
        assert_eq!(one(b"\x1b[32u").text, Some(' '), "kitty space types a space");
        assert_eq!(one(b"\x1b[3~").text, None, "delete is not text");
    }

    #[test]
    fn only_a_lone_escape_asks_for_the_short_wait() {
        let mut decoder = Decoder::new();
        decoder.feed(b"\x1b");
        assert!(decoder.waiting_on_lone_escape());
        decoder.feed(b"[");
        assert!(!decoder.waiting_on_lone_escape(), "an introducer is provably unfinished");
        assert!(decoder.has_partial());

        let mut utf8 = Decoder::new();
        utf8.feed(&"é".as_bytes()[..1]);
        assert!(utf8.has_partial() && !utf8.waiting_on_lone_escape(), "half a character too");
    }
}
