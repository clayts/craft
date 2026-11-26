//! Semantic highlighting: what the server says each token of a document is,
//! such as a function or a mutable variable, shown as the theme says.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::{Rc, Weak};
use std::time::Duration;

use gtk::glib;
use gtk::prelude::*;
use lsp_types::request::Request;
use lsp_types::{
    PartialResultParams, Position, SemanticTokenModifier, SemanticTokensDeltaParams,
    SemanticTokensFullOptions, SemanticTokensLegend, SemanticTokensParams,
    SemanticTokensServerCapabilities, ServerCapabilities, TextDocumentIdentifier,
    WorkDoneProgressParams,
};
use serde::{Deserialize, Serialize};

use super::client::Error;
use super::document::Document;
use super::position::Cursor;
use crate::theme;

/// How long typing must pause before the tokens are asked for again.
const DELAY: Duration = Duration::from_millis(200);

/// A document's tokens, kept up with its text and shown as tags on it.
pub struct Highlighter {
    document: Weak<Document>,
    /// The tokens last received, with the version of the text they are for.
    tokens: RefCell<Option<(Tokens, i32)>>,
    /// A tag for each look the theme gives tokens.
    tags: RefCell<Vec<(theme::Look, gtk::TextTag)>>,
    /// The tag for each kind of token, by type and modifiers, where the theme
    /// styles it.
    kinds: RefCell<HashMap<(u32, u32), Option<gtk::TextTag>>>,
    /// The tokens the tags show, to tell where later tokens differ.
    shown: RefCell<Option<Vec<u32>>>,
    /// The text edited since the tags were placed, whose tags may have come
    /// apart.
    edited: RefCell<Option<(gtk::TextMark, gtk::TextMark)>>,
    /// Waits for typing to pause.
    timer: RefCell<Option<glib::SourceId>>,
    /// The request under way.
    request: RefCell<Option<glib::JoinHandle<()>>>,
    _theme: theme::Watch,
}

/// Tokens as servers give them: five numbers each, for where the token is
/// relative to the last, its length, its type, and its modifiers' bits.
#[derive(Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Tokens {
    result_id: Option<String>,
    data: Vec<u32>,
}

/// The tokens in full, or as changes to the last.
#[derive(Deserialize, Serialize)]
#[serde(untagged)]
pub enum Delta {
    Full(Tokens),
    Changes(Changes),
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Changes {
    result_id: Option<String>,
    edits: Vec<Edit>,
}

/// A change to the numbers of the last tokens, which needs not keep to whole
/// tokens.
#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Edit {
    start: u32,
    delete_count: u32,
    #[serde(default)]
    data: Vec<u32>,
}

/// `textDocument/semanticTokens/full`, with the tokens as numbers.
enum Full {}

impl Request for Full {
    type Params = SemanticTokensParams;
    type Result = Option<Tokens>;
    const METHOD: &'static str = "textDocument/semanticTokens/full";
}

/// `textDocument/semanticTokens/full/delta`, with the tokens as numbers.
enum FullDelta {}

impl Request for FullDelta {
    type Params = SemanticTokensDeltaParams;
    type Result = Option<Delta>;
    const METHOD: &'static str = "textDocument/semanticTokens/full/delta";
}

impl Highlighter {
    pub fn new(document: Weak<Document>) -> Self {
        let watched = document.clone();
        Self {
            document,
            tokens: RefCell::default(),
            tags: RefCell::default(),
            kinds: RefCell::default(),
            shown: RefCell::default(),
            edited: RefCell::default(),
            timer: RefCell::default(),
            request: RefCell::default(),
            _theme: theme::watch(move || {
                if let Some(document) = watched.upgrade() {
                    document.highlighter().restyle(&document);
                }
            }),
        }
    }

    /// Notes that the text from `start` to `end` of `buffer` is about to
    /// change, so that its tags are placed again.
    pub fn touch(&self, buffer: &sourceview::Buffer, start: &gtk::TextIter, end: &gtk::TextIter) {
        let mut edited = self.edited.borrow_mut();
        if let Some((from, to)) = &*edited {
            if *start < buffer.iter_at_mark(from) {
                buffer.move_mark(from, start);
            }
            if *end > buffer.iter_at_mark(to) {
                buffer.move_mark(to, end);
            }
        } else {
            // Spanning what is inserted at either end.
            let from = buffer.create_mark(None, start, true);
            *edited = Some((from, buffer.create_mark(None, end, false)));
        }
    }

    /// Asks again for the tokens once typing pauses.
    pub fn schedule(&self) {
        let document = self.document.clone();
        let timer = glib::timeout_add_local_once(DELAY, move || {
            if let Some(document) = document.upgrade() {
                let highlighter = document.highlighter();
                highlighter.timer.take();
                highlighter.refresh(&document);
            }
        });
        if let Some(earlier) = self.timer.replace(Some(timer)) {
            earlier.remove();
        }
    }

    /// Asks for `document`'s tokens now, or for how they have changed where
    /// the server can say, and shows them, if they are still for the text as
    /// it is.
    pub fn refresh(&self, document: &Document) {
        if let Some(timer) = self.timer.take() {
            timer.remove();
        }
        let client = Rc::clone(document.client());
        let Some(delta) = client.capabilities().and_then(|capabilities| {
            let (delta, _) = options(&capabilities)?;
            Some(delta)
        }) else {
            return;
        };
        let previous = (self.tokens.borrow().as_ref())
            .and_then(|(tokens, _)| tokens.result_id.clone())
            .filter(|_| delta);
        let text_document = TextDocumentIdentifier::new(document.uri().clone());
        let (weak, version) = (self.document.clone(), document.version());
        let request = glib::spawn_future_local(async move {
            let response = match previous {
                Some(previous_result_id) => {
                    client
                        .request::<FullDelta>(SemanticTokensDeltaParams {
                            text_document,
                            previous_result_id,
                            work_done_progress_params: WorkDoneProgressParams::default(),
                            partial_result_params: PartialResultParams::default(),
                        })
                        .await
                }
                None => client
                    .request::<Full>(SemanticTokensParams {
                        text_document,
                        work_done_progress_params: WorkDoneProgressParams::default(),
                        partial_result_params: PartialResultParams::default(),
                    })
                    .await
                    .map(|tokens| tokens.map(Delta::Full)),
            };
            if let Some(document) = weak.upgrade() {
                document.highlighter().receive(&document, response, version);
            }
        });
        // Only the latest request counts: the server can drop the others.
        if let Some(earlier) = self.request.replace(Some(request)) {
            earlier.abort();
        }
    }

    /// Stops highlighting `buffer`, as the server has stopped, or the
    /// document is going, and takes the tags away.
    pub fn clear(&self, buffer: &sourceview::Buffer) {
        if let Some(timer) = self.timer.take() {
            timer.remove();
        }
        if let Some(request) = self.request.take() {
            request.abort();
        }
        self.tokens.take();
        if let Some((from, to)) = self.edited.take() {
            buffer.delete_mark(&from);
            buffer.delete_mark(&to);
        }
        self.forget_tags(buffer);
    }

    /// Keeps the tokens of `response`, a request made at `version`, and shows
    /// them. Where the server failed, or its changes do not apply, what shows
    /// stays, and the next request asks for all the tokens.
    fn receive(&self, document: &Document, response: Result<Option<Delta>, Error>, version: i32) {
        let last = self.tokens.take();
        let tokens = match response {
            Ok(Some(Delta::Full(tokens))) => tokens,
            Ok(Some(Delta::Changes(changes))) => {
                match last.and_then(|(last, _)| last.changed(changes)) {
                    Some(tokens) => tokens,
                    None => return,
                }
            }
            Ok(None) => Tokens::default(),
            Err(_) => return,
        };
        self.tokens.replace(Some((tokens, version)));
        self.highlight(document);
    }

    /// Tags `document`'s text by its tokens, if they are for the text as it
    /// is: anew where the tokens changed or the text was edited, as elsewhere
    /// the tags have kept to the text they were placed on.
    fn highlight(&self, document: &Document) {
        let tokens = self.tokens.borrow();
        let Some((tokens, version)) = tokens.as_ref() else {
            return;
        };
        let capabilities = document.client().capabilities();
        let Some((_, legend)) = capabilities.as_deref().and_then(options) else {
            return;
        };
        if *version != document.version() {
            // Tokens for the text as it is are on their way.
            return;
        }
        let buffer = document.buffer();
        let new = tokens.data.as_chunks().0;
        let placed = place(new);
        let shown = self.shown.replace(Some(tokens.data.clone()));
        let changed = match shown {
            Some(shown) => changed(shown.as_chunks().0, new, &placed),
            None => Some((0, u32::MAX)),
        };
        let edited = self.edited.take().map(|(from, to)| {
            let lines = (
                line(&buffer.iter_at_mark(&from)),
                line(&buffer.iter_at_mark(&to)),
            );
            buffer.delete_mark(&from);
            buffer.delete_mark(&to);
            lines
        });
        let Some((first, last)) = (match (changed, edited) {
            (Some(a), Some(b)) => Some((a.0.min(b.0), a.1.max(b.1))),
            (lines, None) | (None, lines) => lines,
        }) else {
            return;
        };
        let start = iter_at_line(buffer, first);
        let end = iter_at_line(buffer, last.saturating_add(1));
        for (_, tag) in &*self.tags.borrow() {
            buffer.remove_tag(tag, &start, &end);
        }
        let mut cursor = Cursor::new(buffer.upcast_ref(), document.encoding());
        let from = placed.partition_point(|token| token.position.line < first);
        for token in placed[from..]
            .iter()
            .take_while(|token| token.position.line <= last)
        {
            if let Some(tag) = self.tag(document, legend, token.kind, token.modifiers) {
                let start = token.position;
                let end = Position::new(start.line, start.character.saturating_add(token.length));
                buffer.apply_tag(&tag, &cursor.iter_at(start), &cursor.iter_at(end));
            }
        }
    }

    /// The tag for tokens of type `kind` with `modifiers`, as an index into
    /// and bits for `legend`, if the theme styles them.
    fn tag(
        &self,
        document: &Document,
        legend: &SemanticTokensLegend,
        kind: u32,
        modifiers: u32,
    ) -> Option<gtk::TextTag> {
        let mut kinds = self.kinds.borrow_mut();
        let tag = kinds.entry((kind, modifiers)).or_insert_with(|| {
            let kind = legend.token_types.get(kind as usize)?;
            let modifiers: Vec<&str> = (legend.token_modifiers.iter().enumerate())
                .filter(|&(bit, _)| bit < 32 && modifiers & (1 << bit) != 0)
                .map(|(_, modifier)| SemanticTokenModifier::as_str(modifier))
                .collect();
            let look = theme::look(kind.as_str(), &modifiers);
            (!look.is_plain()).then(|| self.tag_for(document, look))
        });
        tag.clone()
    }

    /// The tag giving text `look`, shared by the kinds of token that have it.
    fn tag_for(&self, document: &Document, look: theme::Look) -> gtk::TextTag {
        let mut tags = self.tags.borrow_mut();
        if let Some((_, tag)) = tags.iter().find(|(own, _)| *own == look) {
            return tag.clone();
        }
        let tag = look.tag();
        document.buffer().tag_table().add(&tag);
        document.rank_below_diagnostics(&tag);
        tags.push((look, tag.clone()));
        tag
    }

    /// Shows the tokens styled anew, as the theme or the mode has changed.
    fn restyle(&self, document: &Document) {
        self.forget_tags(document.buffer());
        self.highlight(document);
    }

    /// Takes the tags away, and with them what they showed.
    fn forget_tags(&self, buffer: &sourceview::Buffer) {
        let table = buffer.tag_table();
        for (_, tag) in self.tags.take() {
            table.remove(&tag);
        }
        self.kinds.take();
        self.shown.take();
    }
}

/// A token where it is in the text.
struct Token {
    position: Position,
    length: u32,
    kind: u32,
    modifiers: u32,
}

/// Where in the text `tokens` are, each having been given from the last.
fn place(tokens: &[[u32; 5]]) -> Vec<Token> {
    let mut position = Position::new(0, 0);
    (tokens.iter())
        .map(|&[line, start, length, kind, modifiers]| {
            position = if line == 0 {
                Position::new(position.line, position.character.saturating_add(start))
            } else {
                Position::new(position.line.saturating_add(line), start)
            };
            Token {
                position,
                length,
                kind,
                modifiers,
            }
        })
        .collect()
}

/// The lines from the first to the last where `new` tokens, `placed`,
/// differ from `old` ones: from the line of the last token alike before
/// they differ to that of the first alike after. Tokens are alike that are
/// the same as given, from the last, so that those after an edit are alike
/// though the edit moved them.
fn changed(old: &[[u32; 5]], new: &[[u32; 5]], placed: &[Token]) -> Option<(u32, u32)> {
    let before = (old.iter().zip(new))
        .take_while(|(old, new)| old == new)
        .count();
    if before == old.len() && before == new.len() {
        return None;
    }
    let after = (old[before..].iter().rev())
        .zip(new[before..].iter().rev())
        .take_while(|(old, new)| old == new)
        .count();
    let first = before
        .checked_sub(1)
        .map_or(0, |index| placed[index].position.line);
    let last = (placed.get(new.len() - after)).map_or(u32::MAX, |token| token.position.line);
    Some((first, last))
}

fn line(iter: &gtk::TextIter) -> u32 {
    iter.line().unsigned_abs()
}

/// The start of line `line`, or the end of `buffer` if it has no such line.
fn iter_at_line(buffer: &sourceview::Buffer, line: u32) -> gtk::TextIter {
    i32::try_from(line)
        .ok()
        .and_then(|line| buffer.iter_at_line(line))
        .unwrap_or_else(|| buffer.end_iter())
}

impl Tokens {
    /// The tokens after `changes`, if they apply.
    fn changed(mut self, changes: Changes) -> Option<Self> {
        let mut edits = changes.edits;
        edits.sort_by_key(|edit| edit.start);
        // From the last, so that each edit's place is as it was.
        for edit in edits.into_iter().rev() {
            let start = edit.start as usize;
            let end = (start.checked_add(edit.delete_count as usize))
                .filter(|&end| end <= self.data.len())?;
            self.data.splice(start..end, edit.data);
        }
        self.result_id = changes.result_id;
        Some(self)
    }
}

/// Whether the server gives changes to its tokens, if it gives tokens at all,
/// with the legend that says what their numbers mean.
fn options(capabilities: &ServerCapabilities) -> Option<(bool, &SemanticTokensLegend)> {
    let options = match capabilities.semantic_tokens_provider.as_ref()? {
        SemanticTokensServerCapabilities::SemanticTokensOptions(options) => options,
        SemanticTokensServerCapabilities::SemanticTokensRegistrationOptions(options) => {
            &options.semantic_tokens_options
        }
    };
    let delta = match options.full.as_ref()? {
        SemanticTokensFullOptions::Bool(full) => full.then_some(false)?,
        SemanticTokensFullOptions::Delta { delta } => delta.unwrap_or(false),
    };
    Some((delta, &options.legend))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens(data: &[u32]) -> Tokens {
        Tokens {
            result_id: Some("1".to_owned()),
            data: data.to_vec(),
        }
    }

    fn changes(json: &str) -> Changes {
        match serde_json::from_str(json).unwrap() {
            Delta::Changes(changes) => changes,
            Delta::Full(_) => panic!("read changes as tokens"),
        }
    }

    #[test]
    fn changes_apply_from_the_last_and_need_not_keep_to_tokens() {
        let changes = changes(
            r#"{"resultId": "2", "edits": [
                {"start": 7, "deleteCount": 1, "data": [9, 9]},
                {"start": 1, "deleteCount": 2}
            ]}"#,
        );
        let tokens = tokens(&[0, 1, 2, 3, 4, 5, 6, 7, 8, 9])
            .changed(changes)
            .unwrap();
        assert_eq!(tokens.data, [0, 3, 4, 5, 6, 9, 9, 8, 9]);
        assert_eq!(tokens.result_id.as_deref(), Some("2"));
    }

    #[test]
    fn changes_beyond_the_tokens_do_not_apply() {
        let changes = changes(r#"{"edits": [{"start": 4, "deleteCount": 2, "data": []}]}"#);
        assert!(tokens(&[1, 2, 3, 4, 5]).changed(changes).is_none());
    }

    #[test]
    fn tokens_are_placed_from_the_last() {
        let placed = place(&[[2, 4, 3, 0, 0], [0, 6, 1, 1, 0], [1, 2, 5, 2, 1]]);
        let positions: Vec<(u32, u32)> = (placed.iter())
            .map(|token| (token.position.line, token.position.character))
            .collect();
        assert_eq!(positions, [(2, 4), (2, 10), (3, 2)]);
    }

    #[test]
    fn changes_span_the_lines_from_the_last_token_alike_to_the_next() {
        let old = [[0, 0, 1, 0, 0], [1, 0, 1, 0, 0], [2, 0, 1, 0, 0]];
        // A line inserted before the second token moves it and the third,
        // which is alike, now on line 4.
        let new = [[0, 0, 1, 0, 0], [2, 0, 1, 0, 0], [2, 0, 1, 0, 0]];
        assert_eq!(changed(&old, &new, &place(&new)), Some((0, 4)));
        // The last token's kind changing leaves the lines before it be.
        let new = [[0, 0, 1, 0, 0], [1, 0, 1, 0, 0], [2, 0, 1, 7, 0]];
        assert_eq!(changed(&old, &new, &place(&new)), Some((1, u32::MAX)));
        // The first token going takes in the line of the next.
        let new = [[1, 0, 1, 0, 0], [2, 0, 1, 0, 0]];
        assert_eq!(changed(&old, &new, &place(&new)), Some((0, 1)));
        assert_eq!(changed(&old, &old, &place(&old)), None);
        assert_eq!(changed(&old, &[], &[]), Some((0, u32::MAX)));
    }

    #[test]
    fn full_tokens_read_as_such() {
        let delta: Delta = serde_json::from_str(r#"{"resultId": "3", "data": [0, 1]}"#).unwrap();
        assert!(matches!(delta, Delta::Full(Tokens { data, .. }) if data == [0, 1]));
    }
}
