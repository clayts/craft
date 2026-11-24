//! Conversion between buffer iterators and LSP positions.

use gtk::prelude::*;
use lsp_types::{Position, PositionEncodingKind};

/// The unit in which a server counts characters within a line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Encoding {
    Utf8,
    Utf16,
}

impl Encoding {
    /// The encoding a server chose; LSP's default is UTF-16.
    pub fn new(kind: Option<&PositionEncodingKind>) -> Self {
        if kind == Some(&PositionEncodingKind::UTF8) {
            Self::Utf8
        } else {
            Self::Utf16
        }
    }

    pub fn position(self, iter: &gtk::TextIter) -> Position {
        let character = match self {
            Self::Utf8 => iter.line_index().unsigned_abs(),
            Self::Utf16 => {
                let mut start = *iter;
                start.set_line_offset(0);
                utf16_len(&start.slice(iter))
            }
        };
        Position::new(iter.line().unsigned_abs(), character)
    }

    /// The iterator at `position`, clamped to the buffer, its lines and their
    /// characters.
    pub fn iter_at(self, buffer: &impl IsA<gtk::TextBuffer>, position: Position) -> gtk::TextIter {
        let line = i32::try_from(position.line).unwrap_or(i32::MAX);
        let Some(mut iter) = buffer.iter_at_line(line) else {
            return buffer.end_iter();
        };
        let mut end = iter;
        if !end.ends_line() {
            end.forward_to_line_end();
        }
        let index = byte_index(&iter.slice(&end), self, position.character);
        iter.set_line_index(i32::try_from(index).unwrap_or(i32::MAX));
        iter
    }
}

/// The byte index in `line` of the character `character` code units in,
/// rounded down to a character boundary and clamped to the line.
fn byte_index(line: &str, encoding: Encoding, character: u32) -> usize {
    let character = character as usize;
    match encoding {
        Encoding::Utf8 => (0..=character.min(line.len()))
            .rev()
            .find(|&index| line.is_char_boundary(index))
            .unwrap_or(0),
        Encoding::Utf16 => {
            let mut units = 0;
            for (index, c) in line.char_indices() {
                units += c.len_utf16();
                if units > character {
                    return index;
                }
            }
            line.len()
        }
    }
}

fn utf16_len(text: &str) -> u32 {
    u32::try_from(text.encode_utf16().count()).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Two bytes in UTF-8 and one unit in UTF-16, then four bytes and two units.
    const LINE: &str = "aé😀b";

    #[test]
    fn utf8_indices_round_down_to_characters() {
        let indices: Vec<_> = (0..=9)
            .map(|c| byte_index(LINE, Encoding::Utf8, c))
            .collect();
        assert_eq!(indices, [0, 1, 1, 3, 3, 3, 3, 7, 8, 8]);
    }

    #[test]
    fn utf16_indices_round_down_to_characters() {
        let indices: Vec<_> = (0..=6)
            .map(|c| byte_index(LINE, Encoding::Utf16, c))
            .collect();
        assert_eq!(indices, [0, 1, 3, 3, 7, 8, 8]);
    }

    #[test]
    fn utf16_length_counts_surrogate_pairs() {
        assert_eq!(utf16_len(LINE), 5);
        assert_eq!(utf16_len(""), 0);
    }

    #[test]
    fn negotiated_encoding_defaults_to_utf16() {
        assert_eq!(
            Encoding::new(Some(&PositionEncodingKind::UTF8)),
            Encoding::Utf8
        );
        assert_eq!(
            Encoding::new(Some(&PositionEncodingKind::UTF16)),
            Encoding::Utf16
        );
        assert_eq!(Encoding::new(None), Encoding::Utf16);
    }
}
