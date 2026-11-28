//! Searching a project's files on another thread, as `GtkSourceView`
//! searches a file, but for those that `.gitignore` files and the like
//! leave out.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::{fs, thread};

use futures_channel::mpsc;
use gtk::prelude::*;
use gtk::{gio, glib};
use sourceview::prelude::*;

use crate::file_tree::HIDDEN;

/// How many matches a search finds at most, to keep its results in hand.
pub const LIMIT: usize = 10_000;

/// How many leading bytes of a file are checked for a NUL, which only binary
/// files have.
const SNIFF_LEN: u64 = 8192;

/// How many characters of a line a match shows before it, the rest giving
/// way to an ellipsis, so that it shows in a narrow sidebar.
const CONTEXT: usize = 12;

/// How many characters of a line a match shows after it, at most.
const TAIL: usize = 200;

/// What to find, as a find bar's settings say.
#[derive(Clone, Debug)]
pub struct Query {
    pub text: String,
    pub case_sensitive: bool,
    pub whole_words: bool,
    pub regex: bool,
}

impl Query {
    pub fn new(settings: &sourceview::SearchSettings) -> Self {
        Self {
            text: settings.search_text().unwrap_or_default().into(),
            case_sensitive: settings.is_case_sensitive(),
            whole_words: settings.is_at_word_boundaries(),
            regex: settings.is_regex_enabled(),
        }
    }

    /// Search settings for the query, for `GtkSourceView` to search a buffer.
    pub fn settings(&self) -> sourceview::SearchSettings {
        let settings = sourceview::SearchSettings::new();
        settings.set_search_text(Some(&self.text));
        settings.set_case_sensitive(self.case_sensitive);
        settings.set_at_word_boundaries(self.whole_words);
        settings.set_regex_enabled(self.regex);
        settings
    }

    /// The query as a regular expression that matches what `GtkSourceView`'s
    /// search does: plain text matches as written, and a whole word as one
    /// that starts and ends a word.
    pub fn regex(&self) -> Result<glib::Regex, glib::Error> {
        let pattern = match (self.regex, self.whole_words) {
            (true, false) => self.text.clone(),
            (true, true) => format!("\\b{}\\b", self.text),
            (false, false) => glib::Regex::escape_string(&self.text).into(),
            (false, true) => format!(
                "(?<!\\w)(?=\\w){}(?<=\\w)(?!\\w)",
                glib::Regex::escape_string(&self.text)
            ),
        };
        let mut flags = glib::RegexCompileFlags::MULTILINE | glib::RegexCompileFlags::OPTIMIZE;
        if !self.case_sensitive {
            flags |= glib::RegexCompileFlags::CASELESS;
        }
        glib::Regex::new(&pattern, flags, glib::RegexMatchFlags::NOTEMPTY)
            .map(|regex| regex.expect("patterns that compile make regexes"))
    }
}

/// A match: where it starts and ends, as a line and a character in it,
/// counted from 0, and its first line, to show.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hit {
    pub start: (i32, i32),
    pub end: (i32, i32),
    /// The match's first line, as Pango markup with the match in bold.
    pub markup: String,
}

/// A file's matches.
pub struct Found {
    pub path: PathBuf,
    pub hits: Vec<Hit>,
}

/// Searches the files under `root` for `query` on another thread, but for
/// those in `open`, whose texts it searches in their place. Sends each file's
/// matches in turn, in the order of the files' paths, until all are searched,
/// `cancellable` is cancelled, or more than [`LIMIT`] matches are found.
pub fn scan(
    root: PathBuf,
    query: Query,
    mut open: HashMap<PathBuf, String>,
    cancellable: gio::Cancellable,
) -> mpsc::UnboundedReceiver<Found> {
    let (sender, receiver) = mpsc::unbounded();
    thread::spawn(move || {
        let Ok(regex) = query.regex() else {
            return;
        };
        let files = ignore::WalkBuilder::new(root)
            // Hidden files are files of the project too, which the tree
            // shows, unlike version control metadata.
            .hidden(false)
            .require_git(false)
            .filter_entry(|entry| {
                !HIDDEN
                    .iter()
                    .any(|name| entry.file_name() == OsStr::new(name))
            })
            .sort_by_file_name(Ord::cmp)
            .build()
            .flatten()
            .filter(|entry| entry.file_type().is_some_and(|kind| kind.is_file()));
        let mut count = 0;
        for entry in files {
            if cancellable.is_cancelled() || count > LIMIT {
                break;
            }
            let path = entry.into_path();
            let Some(text) = open.remove(&path).or_else(|| read(&path)) else {
                continue;
            };
            let hits = hits(&regex, text);
            if hits.is_empty() {
                continue;
            }
            count += hits.len();
            if sender.unbounded_send(Found { path, hits }).is_err() {
                break;
            }
        }
    });
    receiver
}

/// The text of the file at `path`, unless it is not UTF-8 text.
fn read(path: &Path) -> Option<String> {
    let mut file = fs::File::open(path).ok()?;
    let mut bytes = Vec::new();
    // A binary file shows itself early, so is read no further.
    (&mut file).take(SNIFF_LEN).read_to_end(&mut bytes).ok()?;
    if bytes.contains(&0) {
        return None;
    }
    file.read_to_end(&mut bytes).ok()?;
    String::from_utf8(bytes)
        .ok()
        .filter(|text| !text.contains('\0'))
}

/// The matches of `regex` in `text`.
fn hits(regex: &glib::Regex, text: String) -> Vec<Hit> {
    let text = glib::GString::from(text);
    let mut hits = Vec::new();
    let Ok(matches) = regex.match_full(text.as_gstr(), 0, glib::RegexMatchFlags::empty()) else {
        return hits;
    };
    let mut lines = Lines::new(&text);
    while let Some((start, end)) = (matches.matches()).then(|| matches.fetch_pos(0)).flatten() {
        let (start, end) = (offset(start), offset(end));
        hits.push(Hit {
            start: lines.locate(start),
            end: lines.locate(end),
            markup: markup(&text, start, end),
        });
        if !matches.next().unwrap_or(false) {
            break;
        }
    }
    hits
}

fn offset(position: i32) -> usize {
    usize::try_from(position).expect("matches are in the text")
}

/// Finds the lines and characters of byte offsets into a text, given in
/// order.
struct Lines<'a> {
    text: &'a str,
    /// The last offset given, its line, and where that line starts.
    offset: usize,
    line: i32,
    line_start: usize,
}

impl<'a> Lines<'a> {
    fn new(text: &'a str) -> Self {
        Self {
            text,
            offset: 0,
            line: 0,
            line_start: 0,
        }
    }

    /// The line `offset`, no lower than the last given, is on, and its
    /// character in that line.
    fn locate(&mut self, offset: usize) -> (i32, i32) {
        for (index, _) in self.text[self.offset..offset].match_indices('\n') {
            self.line += 1;
            self.line_start = self.offset + index + 1;
        }
        self.offset = offset;
        let character = self.text[self.line_start..offset].chars().count();
        (self.line, i32::try_from(character).unwrap_or(i32::MAX))
    }
}

/// The line of `text` that `start` is on, as Pango markup with what lies from
/// `start` to `end`, or the line's end, in bold, and without the spaces it
/// starts with. A long line shows only a little of what comes before.
fn markup(text: &str, start: usize, end: usize) -> String {
    let line_start = text[..start].rfind('\n').map_or(0, |index| index + 1);
    let line_end = text[start..]
        .find('\n')
        .map_or(text.len(), |index| start + index);
    let end = end.min(line_end);
    let before = text[line_start..start].trim_start();
    let skipped = before.chars().count().saturating_sub(CONTEXT);
    let before = match before.char_indices().nth(skipped) {
        Some((index, _)) if skipped > 0 => format!("…{}", &before[index..]),
        _ => before.to_owned(),
    };
    let after: String = text[end..line_end]
        .trim_end_matches('\r')
        .chars()
        .take(TAIL)
        .collect();
    format!(
        "{}<b>{}</b>{}",
        glib::markup_escape_text(&before),
        glib::markup_escape_text(text[start..end].trim_end_matches('\r')),
        glib::markup_escape_text(&after)
    )
}

#[cfg(test)]
mod tests {
    use super::{Hit, Lines, Query, hits, markup};

    fn query(text: &str, case_sensitive: bool, whole_words: bool, regex: bool) -> Query {
        Query {
            text: text.to_owned(),
            case_sensitive,
            whole_words,
            regex,
        }
    }

    /// Where `query` matches in `text`.
    fn found(query: &Query, text: &str) -> Vec<((i32, i32), (i32, i32))> {
        hits(&query.regex().unwrap(), text.to_owned())
            .into_iter()
            .map(|hit| (hit.start, hit.end))
            .collect()
    }

    #[test]
    fn lines_and_characters_are_counted() {
        let mut lines = Lines::new("ab\nçd\n\nef");
        assert_eq!(lines.locate(1), (0, 1));
        assert_eq!(lines.locate(3), (1, 0));
        assert_eq!(lines.locate(5), (1, 1));
        assert_eq!(lines.locate(8), (3, 0));
        assert_eq!(lines.locate(10), (3, 2));
    }

    #[test]
    fn plain_text_matches_as_written() {
        let text = "a.b A.B a.bc\nxa.b";
        assert_eq!(
            found(&query("a.b", false, false, false), text),
            [
                ((0, 0), (0, 3)),
                ((0, 4), (0, 7)),
                ((0, 8), (0, 11)),
                ((1, 1), (1, 4))
            ]
        );
        assert_eq!(found(&query("a.b", true, false, false), text).len(), 3);
        assert_eq!(
            found(&query("a.b", true, true, false), text),
            [((0, 0), (0, 3))]
        );
    }

    #[test]
    fn whole_words_start_and_end_words() {
        let text = "f(x) g (x) (x)";
        assert_eq!(found(&query("(x", true, true, false), text), []);
        assert_eq!(
            found(&query("x", true, true, false), text),
            [((0, 2), (0, 3)), ((0, 8), (0, 9)), ((0, 12), (0, 13))]
        );
    }

    #[test]
    fn regexes_match_across_lines_but_never_empty() {
        let text = "fn a() {\n}\nfn b() {}\n";
        assert_eq!(
            found(&query(r"\{\n\}", true, false, true), text),
            [((0, 7), (1, 1))]
        );
        assert_eq!(
            found(&query("^fn", true, false, true), text),
            [((0, 0), (0, 2)), ((2, 0), (2, 2))]
        );
        assert_eq!(found(&query("x*", true, false, true), text), []);
        assert!(query("(", true, false, true).regex().is_err());
    }

    #[test]
    fn matches_show_on_their_lines() {
        assert_eq!(
            markup("  let a = <b>;\r\n", 6, 7),
            "let <b>a</b> = &lt;b&gt;;"
        );
        assert_eq!(markup("one\ntwo\nthree", 5, 10), "t<b>wo</b>");
        let long = format!("{}x", "0123456789".repeat(5));
        assert_eq!(markup(&long, 50, 51), "…890123456789<b>x</b>");
    }

    #[test]
    fn hits_hold_their_lines() {
        assert_eq!(
            hits(
                &query("b", true, false, false).regex().unwrap(),
                "a\n  b c".to_owned()
            ),
            [Hit {
                start: (1, 2),
                end: (1, 3),
                markup: "<b>b</b> c".to_owned()
            }]
        );
    }
}
