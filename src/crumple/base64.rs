//! Bits as text: packed six to a character into URL-safe base64, and read back out — with the two
//! ways a crumpled form writes numbers: Elias gamma codes inside a bit stream, for the lengths of
//! runs, and base64 varints on their own, for the colour plane's pairs.
//!
//! Base64 rather than anything denser in characters, such as the 256 braille patterns: what a
//! repository, a disk and a diff all count is BYTES, and a braille character is three of them in
//! UTF-8 for eight bits, where a base64 character is one byte for six.

/// The URL-safe base64 alphabet — letters, digits, `-` and `_` (RFC 4648, section 5) — with no
/// `=` padding. So a line of it never starts with `===`, which keeps a crumpled form's section
/// markers unmistakable.
const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// The character for a six-bit value.
pub(super) fn digit(value: u32) -> char {
    ALPHABET[value as usize & 63] as char
}

/// The six-bit value of a base64 character, if it is one.
pub(super) fn value(c: u8) -> Option<u32> {
    let value = match c {
        b'A'..=b'Z' => c - b'A',
        b'a'..=b'z' => c - b'a' + 26,
        b'0'..=b'9' => c - b'0' + 52,
        b'-' => 62,
        b'_' => 63,
        _ => return None,
    };
    Some(value as u32)
}

/// Bits, most significant first, turned into base64 as they come.
#[derive(Debug, Default)]
pub(super) struct Writer {
    out: String,
    pending: u64,
    held: u32,
}

impl Writer {
    /// The low `count` bits of `value`, at most 32 of them.
    pub(super) fn bits(&mut self, value: u64, count: u32) {
        debug_assert!(count <= 32, "{count} bits at once");
        self.pending = (self.pending << count) | (value & ((1u64 << count) - 1));
        self.held += count;
        while self.held >= 6 {
            self.held -= 6;
            self.out.push(digit((self.pending >> self.held) as u32));
        }
        self.pending &= (1u64 << self.held) - 1;
    }

    /// `m`, at least 1, as an Elias gamma code: as many zeros as `m` has bits after its first,
    /// then `m` itself. Small numbers are short, and no number has a limit.
    pub(super) fn gamma(&mut self, m: u64) {
        debug_assert!(m >= 1, "gamma codes start at 1");
        let width = 64 - m.leading_zeros();
        let mut zeros = width - 1;
        while zeros > 0 {
            let some = zeros.min(32);
            self.bits(0, some);
            zeros -= some;
        }
        let (high, low) = (width.saturating_sub(32), width.min(32));
        if high > 0 {
            self.bits(m >> 32, high);
        }
        self.bits(m, low);
    }

    /// The text written, its last character filled out with zeros.
    pub(super) fn finish(mut self) -> String {
        if self.held > 0 {
            let fill = 6 - self.held;
            self.bits(0, fill);
        }
        self.out
    }
}

/// Bits read back out of base64 text, most significant first.
#[derive(Debug)]
pub(super) struct Reader<'a> {
    text: &'a [u8],
    next: usize,
    pending: u32,
    held: u32,
}

impl<'a> Reader<'a> {
    /// A reader of `text` — `None` if any of it is not base64.
    pub(super) fn new(text: &'a str) -> Option<Self> {
        let text = text.as_bytes();
        text.iter().all(|c| value(*c).is_some()).then_some(Self {
            text,
            next: 0,
            pending: 0,
            held: 0,
        })
    }

    /// The next bit, `None` at the end.
    pub(super) fn bit(&mut self) -> Option<u32> {
        if self.held == 0 {
            let c = *self.text.get(self.next)?;
            self.next += 1;
            self.pending = value(c).expect("checked when the reader was made");
            self.held = 6;
        }
        self.held -= 1;
        Some((self.pending >> self.held) & 1)
    }

    /// The next `count` bits, at most 63, as a number.
    pub(super) fn bits(&mut self, count: u32) -> Option<u64> {
        (0..count).try_fold(0u64, |so_far, _| Some((so_far << 1) | self.bit()? as u64))
    }

    /// An Elias gamma code — `None` at the end, or past `limit` bits of number, which no drawing a
    /// crumpled form can hold would need.
    pub(super) fn gamma(&mut self, limit: u32) -> Option<u64> {
        let mut zeros = 0;
        while self.bit()? == 0 {
            zeros += 1;
            if zeros >= limit {
                return None;
            }
        }
        Some((1u64 << zeros) | self.bits(zeros)?)
    }

    /// Whether all that is left is the zeros that fill out the last character — which a writer
    /// puts there, and nothing else.
    pub(super) fn only_filling_left(&self) -> bool {
        self.next == self.text.len() && self.pending & ((1u32 << self.held) - 1) == 0
    }
}

/// `value` as a base64 varint: five bits to a character, the lowest first, each character's
/// sixth bit saying whether another follows. Values below 32 are one character.
pub(super) fn push_varint(out: &mut String, mut value: u64) {
    loop {
        let low = (value & 31) as u32;
        value >>= 5;
        if value == 0 {
            out.push(digit(low));
            return;
        }
        out.push(digit(low | 32));
    }
}

/// Varints read back out of base64 text, one at a time — see [`push_varint`].
#[derive(Debug)]
pub(super) struct Varints<'a> {
    text: std::slice::Iter<'a, u8>,
}

impl<'a> Varints<'a> {
    pub(super) fn new(text: &'a str) -> Self {
        Self { text: text.as_bytes().iter() }
    }

    /// How many bytes of the text are still to be read.
    pub(super) fn remaining(&self) -> usize {
        self.text.len()
    }
}

impl Iterator for Varints<'_> {
    /// `Err` for a character that is not base64, a varint cut off by the end, or one too long
    /// for a `u64`.
    type Item = Result<u64, ()>;

    fn next(&mut self) -> Option<Self::Item> {
        let mut first = true;
        let (mut number, mut shift) = (0u64, 0u32);
        loop {
            let Some(c) = self.text.next() else {
                return (!first).then_some(Err(()));
            };
            first = false;
            let Some(digit) = value(*c) else { return Some(Err(())) };
            // The thirteenth character has room for only the top four of a u64's bits.
            if shift > 60 || (shift == 60 && digit & 31 > 15) {
                return Some(Err(()));
            }
            number |= ((digit & 31) as u64) << shift;
            shift += 5;
            if digit & 32 == 0 {
                return Some(Ok(number));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every base64 character is one of the 64, and reads back as its value.
    #[test]
    fn every_digit_reads_back_as_its_value() {
        for v in 0..64 {
            assert_eq!(value(digit(v) as u8), Some(v));
        }
        for not in [b'=', b'+', b'/', b' ', b'\n', 0xe2] {
            assert_eq!(value(not), None, "{not:?}");
        }
    }

    /// Bits written come back in the order they went, whatever their grouping, and what fills
    /// out the last character is zeros and nothing else.
    #[test]
    fn bits_come_back_in_the_order_they_went() {
        let groups: &[(u64, u32)] =
            &[(1, 1), (0, 1), (0b101, 3), (0xffff_ffff, 32), (0, 7), (5, 3)];
        let mut writer = Writer::default();
        for &(v, n) in groups {
            writer.bits(v, n);
        }
        let text = writer.finish();
        let total: u32 = groups.iter().map(|(_, n)| n).sum();
        assert_eq!(text.len() as u32, total.div_ceil(6));
        let mut reader = Reader::new(&text).expect("base64");
        for &(v, n) in groups {
            assert_eq!(reader.bits(n), Some(v), "{n} bits of {v}");
        }
        assert!(reader.only_filling_left());
    }

    /// Gamma codes round-trip across the range, the smallest and the largest included, and a
    /// reader refuses one longer than it was told to allow.
    #[test]
    fn gamma_codes_come_back_and_are_bounded() {
        let numbers = [1, 2, 3, 4, 7, 8, 255, 256, 10_000, (1 << 40) + 3, u64::MAX >> 1];
        let mut writer = Writer::default();
        for &m in &numbers {
            writer.gamma(m);
        }
        let text = writer.finish();
        let mut reader = Reader::new(&text).expect("base64");
        for &m in &numbers {
            assert_eq!(reader.gamma(64), Some(m));
        }
        let mut long = Writer::default();
        long.gamma(1 << 20);
        assert_eq!(Reader::new(&long.finish()).unwrap().gamma(20), None, "past the limit");
    }

    /// Anything that is not base64 is refused before a bit is read, and a stream that is not
    /// zeros to its end is not only filling.
    #[test]
    fn a_reader_refuses_what_is_not_base64_and_sees_unclean_filling() {
        assert!(Reader::new("AB=").is_none());
        assert!(Reader::new("A\u{2800}").is_none());
        let mut reader = Reader::new("B").expect("base64"); // 000001
        assert_eq!(reader.bits(3), Some(0));
        assert!(!reader.only_filling_left(), "a one is still to come");
    }

    /// Varints round-trip, one character below 32, and a cut-off or overlong one is refused.
    #[test]
    fn varints_come_back_and_refuse_what_is_broken() {
        let numbers = [0, 1, 31, 32, 1023, 1024, 99_999, u64::MAX];
        let mut text = String::new();
        for &n in &numbers {
            push_varint(&mut text, n);
        }
        let back: Vec<u64> = Varints::new(&text).collect::<Result<_, _>>().expect("well formed");
        assert_eq!(back, numbers);
        let mut one = String::new();
        push_varint(&mut one, 31);
        assert_eq!(one.len(), 1);
        assert_eq!(
            Varints::new("g").collect::<Vec<_>>(),
            [Err(())],
            "cut off after a continuation"
        );
        assert_eq!(Varints::new("=").collect::<Vec<_>>(), [Err(())], "not base64");
        assert!(Varints::new(&"_".repeat(14)).any(|v| v.is_err()), "too long for 64 bits");
        assert_eq!(Varints::new("").count(), 0);
    }
}
