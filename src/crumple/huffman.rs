//! Canonical Huffman codes. A code length for each symbol is all a crumpled form stores: the codes
//! follow from the lengths alone — shorter codes first, those of one length in symbol order,
//! counting up — so the table costs a character a symbol, and two builds agree on every bit.

use super::base64::Reader;
use std::cmp::Reverse;
use std::collections::BinaryHeap;

/// The longest code allowed, so a code always fits the reader's arithmetic. Only counts shaped
/// like the Fibonacci numbers, over millions of symbols, ever need longer; past it, the encoder
/// gives the version up for the plain one rather than write what could not be read.
pub(super) const MAX_LENGTH: u32 = 32;

/// The code length each symbol gets for its count — 0 for one that never occurs — built the
/// classic way, by merging the two rarest over and over. Ties go the same way every time, to the
/// earlier symbol, so the same counts always give the same code. `None` when the longest code
/// would pass [`MAX_LENGTH`].
pub(super) fn lengths(counts: &[u64]) -> Option<Vec<u32>> {
    let mut lengths = vec![0; counts.len()];
    let used: Vec<usize> = (0..counts.len()).filter(|&symbol| counts[symbol] > 0).collect();
    if let [only] = used[..] {
        // One symbol still needs a bit to say it is there.
        lengths[only] = 1;
        return Some(lengths);
    }
    // Each group is the symbols under one node of the tree; merging two puts them all a level
    // deeper. The heap orders groups by count, then by when they were made.
    let mut groups: Vec<Vec<usize>> = used.iter().map(|&symbol| vec![symbol]).collect();
    let mut heap: BinaryHeap<Reverse<(u64, usize)>> =
        used.iter().enumerate().map(|(group, &symbol)| Reverse((counts[symbol], group))).collect();
    while let (Some(Reverse((a, first))), Some(Reverse((b, second)))) = (heap.pop(), heap.pop()) {
        let mut merged = std::mem::take(&mut groups[first]);
        merged.append(&mut std::mem::take(&mut groups[second]));
        for &symbol in &merged {
            lengths[symbol] += 1;
        }
        heap.push(Reverse((a + b, groups.len())));
        groups.push(merged);
    }
    (lengths.iter().all(|&length| length <= MAX_LENGTH)).then_some(lengths)
}

/// The code each symbol's length gives it, canonically: the shortest first, counting up, and
/// one length's symbols in their order. A symbol of length 0 has no code, and what it is given
/// here is never used.
pub(super) fn codes(lengths: &[u32]) -> Vec<u64> {
    let mut codes = vec![0; lengths.len()];
    let (mut code, mut length) = (0u64, 0u32);
    for symbol in by_code(lengths) {
        code <<= lengths[symbol] - length;
        length = lengths[symbol];
        codes[symbol] = code;
        code += 1;
    }
    codes
}

/// The symbols that have codes, in the order their codes count up in.
fn by_code(lengths: &[u32]) -> Vec<usize> {
    let mut symbols: Vec<usize> = (0..lengths.len()).filter(|&s| lengths[s] > 0).collect();
    symbols.sort_by_key(|&symbol| (lengths[symbol], symbol));
    symbols
}

/// Reads canonical codes back into symbols, a bit at a time — the way `puff`, zlib's reference
/// inflater, does it: at each length, whether the bits so far fall among that length's codes.
#[derive(Debug)]
pub(super) struct Decoder {
    /// How many codes there are of each length.
    count: [u64; MAX_LENGTH as usize + 1],
    /// The symbols in the order their codes count up in.
    symbols: Vec<usize>,
}

impl Decoder {
    /// A decoder for these lengths — `None` unless they are a COMPLETE prefix code, as Huffman's
    /// always is: every bit pattern the start of exactly one code, which is Kraft's equality. A
    /// code with more than that is impossible, and one with less leaves patterns that are no
    /// symbol's, which only a damaged table would have. The one exception is a lone symbol, whose
    /// one-bit code leaves the other bit unused. A length past [`MAX_LENGTH`] is refused too.
    pub(super) fn new(lengths: &[u32]) -> Option<Self> {
        let mut count = [0u64; MAX_LENGTH as usize + 1];
        for &length in lengths {
            match length {
                0 => {}
                length if length <= MAX_LENGTH => count[length as usize] += 1,
                _ => return None,
            }
        }
        let mut room = 1u64;
        for &codes in &count[1..] {
            room = (room << 1).checked_sub(codes)?;
        }
        let lone = count[1] == 1 && count[2..].iter().all(|&codes| codes == 0);
        (room == 0 || lone).then(|| Self { count, symbols: by_code(lengths) })
    }

    /// The next symbol — `None` at the end of the bits, or where they are no symbol's code.
    pub(super) fn decode(&self, reader: &mut Reader<'_>) -> Option<usize> {
        let (mut code, mut first, mut index) = (0u64, 0u64, 0usize);
        for &count in &self.count[1..] {
            code |= reader.bit()? as u64;
            if code < first + count {
                return self.symbols.get(index + (code - first) as usize).copied();
            }
            index += count as usize;
            first = (first + count) << 1;
            code <<= 1;
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::super::base64::Writer;
    use super::*;

    /// The sum over codes of 2^-length: exactly 1 for a complete code, which Huffman's always is
    /// for two symbols or more.
    fn kraft(lengths: &[u32]) -> f64 {
        lengths.iter().filter(|&&l| l > 0).map(|&l| 0.5f64.powi(l as i32)).sum()
    }

    /// A code that is complete, favours what is common, and comes out the same every time.
    #[test]
    fn huffman_lengths_are_complete_and_favour_the_common() {
        let counts = [50, 1, 1, 7, 0, 30, 2, 2, 9];
        let lengths = lengths(&counts).expect("short enough");
        assert_eq!(lengths[4], 0, "a symbol that never occurs has no code");
        assert!((kraft(&lengths) - 1.0).abs() < 1e-12, "complete: {lengths:?}");
        for (a, b) in [(0, 5), (5, 8), (8, 3), (3, 1)] {
            assert!(lengths[a] <= lengths[b], "{a} is commoner than {b}: {lengths:?}");
        }
        assert_eq!(super::lengths(&counts), Some(lengths), "the same counts, the same code");
        let tied = super::lengths(&[3, 3, 3, 3]).unwrap();
        assert_eq!(tied, [2, 2, 2, 2]);
    }

    /// No symbols, no codes; one symbol, a one-bit code.
    #[test]
    fn nothing_and_one_symbol_are_coded_too() {
        assert_eq!(lengths(&[0, 0]), Some(vec![0, 0]));
        assert_eq!(lengths(&[0, 9, 0]), Some(vec![0, 1, 0]));
    }

    /// Counts shaped like the Fibonacci numbers make each code one longer than the last, and past
    /// the longest allowed there is no code at all.
    #[test]
    fn a_code_too_long_is_given_up() {
        let mut fibonacci = vec![1u64, 1];
        while fibonacci.len() < 40 {
            let next = fibonacci[fibonacci.len() - 1] + fibonacci[fibonacci.len() - 2];
            fibonacci.push(next);
        }
        assert_eq!(lengths(&fibonacci), None);
        assert!(lengths(&fibonacci[..20]).is_some());
    }

    /// Whatever is written in the codes reads back as the symbols it was.
    #[test]
    fn codes_read_back_as_their_symbols() {
        let counts = [50, 1, 1, 7, 0, 30, 2, 2, 9, 1, 1, 1];
        let lengths = lengths(&counts).unwrap();
        let codes = codes(&lengths);
        let message: Vec<usize> = (0..300).map(|n| [0, 5, 8, 3, 1, 11, 0, 0, 6][n % 9]).collect();
        let mut writer = Writer::default();
        for &symbol in &message {
            writer.bits(codes[symbol], lengths[symbol]);
        }
        let text = writer.finish();
        let decoder = Decoder::new(&lengths).expect("a prefix code");
        let mut reader = Reader::new(&text).unwrap();
        let back: Vec<usize> =
            (0..message.len()).map(|_| decoder.decode(&mut reader).unwrap()).collect();
        assert_eq!(back, message);
    }

    /// Lengths that are no complete prefix code are refused before anything is read with them:
    /// too many codes, too few — which would leave patterns no symbol owns — or too long.
    #[test]
    fn lengths_that_are_no_complete_prefix_code_are_refused() {
        assert!(Decoder::new(&[1, 1, 1]).is_none(), "three one-bit codes");
        assert!(Decoder::new(&[2, 2, 2, 2, 2]).is_none(), "five two-bit codes");
        assert!(Decoder::new(&[1, 2]).is_none(), "a quarter of the patterns owned by nothing");
        assert!(Decoder::new(&[2, 2, 2]).is_none(), "and here too");
        assert!(Decoder::new(&[2]).is_none(), "a lone symbol's code is one bit long");
        assert!(Decoder::new(&[MAX_LENGTH + 1]).is_none(), "too long to read");
        assert!(Decoder::new(&[1, 2, 2]).is_some());
        assert!(Decoder::new(&[0, 1, 0]).is_some(), "a lone symbol");
    }
}
