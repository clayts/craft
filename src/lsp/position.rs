//! Conversion between buffer iterators and LSP positions, and how edits move
//! positions.

use gtk::prelude::*;
use lsp_types::{Position, PositionEncodingKind, Range};

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

    /// The length of `text` in this encoding's units.
    pub fn units(self, text: &str) -> u32 {
        match self {
            Self::Utf8 => u32::try_from(text.len()).unwrap_or(u32::MAX),
            Self::Utf16 => utf16_len(text),
        }
    }

    /// The iterator at `position`, clamped to the buffer, its lines and their
    /// characters.
    pub fn iter_at(self, buffer: &impl IsA<gtk::TextBuffer>, position: Position) -> gtk::TextIter {
        Cursor::new(buffer.upcast_ref(), self).iter_at(position)
    }

    /// The length of `c` in this encoding's units.
    fn width(self, c: char) -> usize {
        match self {
            Self::Utf8 => c.len_utf8(),
            Self::Utf16 => c.len_utf16(),
        }
    }
}

/// Finds iterators at positions in a buffer, as [`Encoding::iter_at`] does,
/// reading each line's text once for positions given in order through it.
pub struct Cursor<'a> {
    buffer: &'a gtk::TextBuffer,
    encoding: Encoding,
    /// The line last read.
    line: Option<Line>,
}

/// A line of text, and how far into it the cursor is.
struct Line {
    number: u32,
    start: gtk::TextIter,
    text: String,
    /// How far in, in bytes and in the encoding's units.
    at: (usize, usize),
}

impl<'a> Cursor<'a> {
    pub fn new(buffer: &'a gtk::TextBuffer, encoding: Encoding) -> Self {
        Self {
            buffer,
            encoding,
            line: None,
        }
    }

    /// The iterator at `position`, clamped to the buffer, its lines and their
    /// characters.
    pub fn iter_at(&mut self, position: Position) -> gtk::TextIter {
        let character = position.character as usize;
        let line = match &mut self.line {
            Some(line) if line.number == position.line && line.at.1 <= character => line,
            _ => {
                let number = i32::try_from(position.line).unwrap_or(i32::MAX);
                let Some(start) = self.buffer.iter_at_line(number) else {
                    return self.buffer.end_iter();
                };
                let mut end = start;
                if !end.ends_line() {
                    end.forward_to_line_end();
                }
                self.line.insert(Line {
                    number: position.line,
                    start,
                    text: start.slice(&end).into(),
                    at: (0, 0),
                })
            }
        };
        line.at = advance(&line.text, self.encoding, line.at, character);
        let mut iter = line.start;
        iter.set_line_index(i32::try_from(line.at.0).unwrap_or(i32::MAX));
        iter
    }
}

/// Where `character` code units into `line` is, onward from `at`, in bytes
/// and units: rounded down to a character boundary and clamped to the line.
fn advance(line: &str, encoding: Encoding, at: (usize, usize), character: usize) -> (usize, usize) {
    let (mut bytes, mut units) = at;
    for c in line[bytes..].chars() {
        let width = encoding.width(c);
        if units + width > character {
            break;
        }
        bytes += c.len_utf8();
        units += width;
    }
    (bytes, units)
}

fn utf16_len(text: &str) -> u32 {
    u32::try_from(text.encode_utf16().count()).unwrap_or(u32::MAX)
}

/// How an edit moves the positions after it: the text from `start` to `end`
/// was replaced by text ending at `inserted_end`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Shift {
    start: Position,
    end: Position,
    inserted_end: Position,
}

impl Shift {
    /// The shift made by replacing `range` with `text`, measured in
    /// `encoding`.
    pub fn new(range: Range, text: &str, encoding: Encoding) -> Self {
        let mut lines = text.rsplit('\n');
        let last_line = lines.next().unwrap_or_default();
        let newlines = u32::try_from(lines.count()).unwrap_or(u32::MAX);
        let inserted_end = if newlines == 0 {
            Position::new(
                range.start.line,
                range.start.character.saturating_add(encoding.units(text)),
            )
        } else {
            Position::new(
                range.start.line.saturating_add(newlines),
                encoding.units(last_line),
            )
        };
        Self {
            start: range.start,
            end: range.end,
            inserted_end,
        }
    }

    /// Where `position`, from before the edit, is after it. Positions in the
    /// replaced text move to its start.
    pub fn apply(self, position: Position) -> Position {
        if position < self.start {
            position
        } else if position < self.end {
            self.start
        } else if position.line == self.end.line {
            let character = position.character - self.end.character;
            Position::new(
                self.inserted_end.line,
                self.inserted_end.character.saturating_add(character),
            )
        } else {
            let line = position.line - self.end.line;
            Position::new(
                self.inserted_end.line.saturating_add(line),
                position.character,
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Two bytes in UTF-8 and one unit in UTF-16, then four bytes and two units.
    const LINE: &str = "aé😀b";

    fn byte_index(line: &str, encoding: Encoding, character: usize) -> usize {
        advance(line, encoding, (0, 0), character).0
    }

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
    fn advancing_goes_on_from_where_it_was() {
        let at = advance(LINE, Encoding::Utf16, (0, 0), 2);
        assert_eq!(at, (3, 2));
        assert_eq!(advance(LINE, Encoding::Utf16, at, 4), (7, 4));
        assert_eq!(advance(LINE, Encoding::Utf8, (1, 1), 100), (8, 8));
    }

    #[test]
    fn utf16_length_counts_surrogate_pairs() {
        assert_eq!(utf16_len(LINE), 5);
        assert_eq!(utf16_len(""), 0);
    }

    fn shift(start: (u32, u32), end: (u32, u32), text: &str) -> Shift {
        let range = Range::new(Position::new(start.0, start.1), Position::new(end.0, end.1));
        Shift::new(range, text, Encoding::Utf16)
    }

    fn moved(shift: Shift, line: u32, character: u32) -> (u32, u32) {
        let position = shift.apply(Position::new(line, character));
        (position.line, position.character)
    }

    #[test]
    fn insertions_push_later_positions_along() {
        let insert = shift((1, 2), (1, 2), "ab");
        assert_eq!(moved(insert, 1, 1), (1, 1));
        assert_eq!(moved(insert, 1, 2), (1, 4));
        assert_eq!(moved(insert, 1, 5), (1, 7));
        assert_eq!(moved(insert, 2, 0), (2, 0));
        let lines = shift((1, 2), (1, 2), "x\nyz");
        assert_eq!(moved(lines, 1, 5), (2, 5));
        assert_eq!(moved(lines, 3, 1), (4, 1));
    }

    #[test]
    fn deletions_pull_later_positions_back() {
        let delete = shift((1, 2), (3, 4), "");
        assert_eq!(moved(delete, 1, 3), (1, 2));
        assert_eq!(moved(delete, 3, 6), (1, 4));
        assert_eq!(moved(delete, 5, 0), (3, 0));
    }

    #[test]
    fn shifts_count_in_the_encoding() {
        let range = Range::new(Position::new(0, 0), Position::new(0, 0));
        assert_eq!(
            Shift::new(range, "😀", Encoding::Utf16)
                .apply(range.end)
                .character,
            2
        );
        assert_eq!(
            Shift::new(range, "😀", Encoding::Utf8)
                .apply(range.end)
                .character,
            4
        );
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
