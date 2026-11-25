//! An open file as its language server sees it: kept in sync with the buffer
//! and marked with the server's diagnostics.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::future::Future;
use std::rc::{Rc, Weak};

use gtk::prelude::*;
use gtk::{gdk, gio, glib, pango};
use lsp_types::notification::{
    DidChangeTextDocument, DidCloseTextDocument, DidOpenTextDocument, DidSaveTextDocument,
};
use lsp_types::request::{Completion, GotoDefinition, HoverRequest, ResolveCompletionItem};
use lsp_types::{
    CompletionContext, CompletionItem, CompletionParams, CompletionResponse, Diagnostic,
    DiagnosticSeverity, DidChangeTextDocumentParams, DidCloseTextDocumentParams,
    DidOpenTextDocumentParams, DidSaveTextDocumentParams, GotoDefinitionParams,
    GotoDefinitionResponse, HoverContents, HoverParams, HoverProviderCapability, MarkedString,
    OneOf, PartialResultParams, Position, Range, ServerCapabilities,
    TextDocumentContentChangeEvent, TextDocumentIdentifier, TextDocumentItem,
    TextDocumentPositionParams, TextDocumentSyncCapability, TextDocumentSyncKind,
    TextDocumentSyncOptions, TextDocumentSyncSaveOptions, Uri, VersionedTextDocumentIdentifier,
    WorkDoneProgressParams,
};
use sourceview::prelude::*;

use super::client::Client;
use super::completion;
use super::hover::HoverProvider;
use super::{Encoding, Shift};
use crate::editor::Editor;

const ERROR: gdk::RGBA = gdk::RGBA::new(0.878, 0.106, 0.141, 1.0);
const WARNING: gdk::RGBA = gdk::RGBA::new(0.898, 0.647, 0.039, 1.0);
const INFORMATION: gdk::RGBA = gdk::RGBA::new(0.208, 0.518, 0.894, 1.0);

/// How many edits to remember for carrying diagnostics forward.
const MAX_EDITS: usize = 1024;

pub struct Document {
    client: Rc<Client>,
    editor: glib::WeakRef<Editor>,
    uri: Uri,
    language: &'static str,
    buffer: sourceview::Buffer,
    view: sourceview::View,
    hover: HoverProvider,
    completion: completion::Provider,
    handlers: Vec<glib::SignalHandlerId>,
    /// Whether the server has the document, which it lacks while starting.
    opened: Cell<bool>,
    version: Cell<i32>,
    /// The edits since the last version the server published diagnostics
    /// for, with the version each made, to carry its later diagnostics
    /// forward.
    edits: RefCell<VecDeque<(i32, Shift)>>,
    /// Each diagnostic's underline, which follows edits, and message.
    diagnostics: RefCell<Vec<(gtk::TextTag, String)>>,
}

impl Document {
    /// Connects `editor`, whose file is in the LSP language `language`, to
    /// `client`.
    pub(super) fn new(client: &Rc<Client>, editor: &Editor, language: &'static str) -> Rc<Self> {
        let buffer = editor.buffer();
        let view = editor.view();
        let document = Rc::new_cyclic(|this: &Weak<Self>| {
            let (insert, delete, changed) = (this.clone(), this.clone(), this.clone());
            let handlers = vec![
                buffer.connect_insert_text(move |_, iter, text| {
                    if let Some(document) = insert.upgrade() {
                        document.edited(iter, iter, text);
                    }
                }),
                buffer.connect_delete_range(move |_, start, end| {
                    if let Some(document) = delete.upgrade() {
                        document.edited(start, end, "");
                    }
                }),
                buffer.connect_changed(move |_| {
                    if let Some(document) = changed.upgrade() {
                        document.replaced();
                    }
                }),
            ];
            Self {
                client: Rc::clone(client),
                editor: editor.downgrade(),
                uri: editor
                    .file()
                    .uri()
                    .parse()
                    .expect("GIO produces valid URIs"),
                language,
                buffer: buffer.clone(),
                view: view.clone(),
                hover: HoverProvider::new(this.clone()),
                completion: completion::Provider::new(this.clone()),
                handlers,
                opened: Cell::new(false),
                version: Cell::new(0),
                edits: RefCell::default(),
                diagnostics: RefCell::default(),
            }
        });
        view.hover().add_provider(&document.hover);
        view.completion().add_provider(&document.completion);
        client.add(&document.uri, &document);
        document
    }

    pub(super) fn editor(&self) -> Option<Editor> {
        self.editor.upgrade()
    }

    pub(super) fn language(&self) -> &'static str {
        self.language
    }

    pub fn encoding(&self) -> Encoding {
        self.client.encoding()
    }

    pub(super) fn buffer(&self) -> &sourceview::Buffer {
        &self.buffer
    }

    pub(super) fn view(&self) -> &sourceview::View {
        &self.view
    }

    /// Gives the server the document, as it now is.
    pub(super) fn open(&self) {
        self.opened.set(true);
        self.version.set(0);
        self.edits.borrow_mut().clear();
        self.client
            .notify::<DidOpenTextDocument>(DidOpenTextDocumentParams {
                text_document: TextDocumentItem::new(
                    self.uri.clone(),
                    self.language.to_owned(),
                    0,
                    self.text(),
                ),
            });
    }

    /// Forgets the server's copy, which it lost by stopping.
    pub(super) fn closed(&self) {
        self.opened.set(false);
        self.clear_diagnostics();
    }

    pub fn saved(&self) {
        let include_text = match self.save_options() {
            Some(include_text) if self.opened.get() => include_text,
            _ => return,
        };
        self.client
            .notify::<DidSaveTextDocument>(DidSaveTextDocumentParams {
                text_document: TextDocumentIdentifier::new(self.uri.clone()),
                text: include_text.then(|| self.text()),
            });
    }

    /// Whether the server wants to hear of saves, and if so, whether with the
    /// text.
    fn save_options(&self) -> Option<bool> {
        let capabilities = self.client.capabilities()?;
        match capabilities.text_document_sync.as_ref()? {
            TextDocumentSyncCapability::Options(TextDocumentSyncOptions {
                save: Some(save),
                ..
            }) => match save {
                TextDocumentSyncSaveOptions::Supported(supported) => supported.then_some(false),
                TextDocumentSyncSaveOptions::SaveOptions(options) => {
                    Some(options.include_text == Some(true))
                }
            },
            _ => None,
        }
    }

    fn sync_kind(&self) -> Option<TextDocumentSyncKind> {
        let capabilities = self.client.capabilities()?;
        match capabilities.text_document_sync.as_ref()? {
            TextDocumentSyncCapability::Kind(kind) => Some(*kind),
            TextDocumentSyncCapability::Options(options) => options.change,
        }
    }

    /// Sends an edit about to be made, for servers that take edits.
    fn edited(&self, start: &gtk::TextIter, end: &gtk::TextIter, text: &str) {
        if self.opened.get() && self.sync_kind() == Some(TextDocumentSyncKind::INCREMENTAL) {
            let encoding = self.client.encoding();
            let range = Range::new(encoding.position(start), encoding.position(end));
            let shift = Shift::new(range, text, encoding);
            let version = self.change(TextDocumentContentChangeEvent {
                range: Some(range),
                range_length: None,
                text: text.to_owned(),
            });
            let mut edits = self.edits.borrow_mut();
            if edits.len() == MAX_EDITS {
                edits.pop_front();
            }
            edits.push_back((version, shift));
        }
    }

    /// Sends the text after an edit, for servers that take whole texts.
    fn replaced(&self) {
        if self.opened.get() && self.sync_kind() == Some(TextDocumentSyncKind::FULL) {
            // Whole texts do not say how positions moved.
            self.edits.borrow_mut().clear();
            self.change(TextDocumentContentChangeEvent {
                range: None,
                range_length: None,
                text: self.text(),
            });
        }
    }

    /// Sends `change`, returning the version of the text it makes.
    fn change(&self, change: TextDocumentContentChangeEvent) -> i32 {
        let version = self.version.get() + 1;
        self.version.set(version);
        self.client
            .notify::<DidChangeTextDocument>(DidChangeTextDocumentParams {
                text_document: VersionedTextDocumentIdentifier::new(self.uri.clone(), version),
                content_changes: vec![change],
            });
        version
    }

    fn text(&self) -> String {
        let (start, end) = self.buffer.bounds();
        self.buffer.text(&start, &end, true).into()
    }

    /// Underlines `diagnostics`, replacing the previous ones. Those for an
    /// earlier version of the text are carried forward through the edits
    /// since, as a server that finds nothing new after them publishes nothing
    /// new.
    pub(super) fn set_diagnostics(&self, version: Option<i32>, diagnostics: Vec<Diagnostic>) {
        if !self.opened.get() {
            return;
        }
        let current = self.version.get();
        let version = version.unwrap_or(current);
        let shifts: Vec<Shift> = {
            let mut edits = self.edits.borrow_mut();
            // Edits up to `version` are reflected in these diagnostics, and
            // so in any later ones.
            while edits.front().is_some_and(|&(edited, _)| edited <= version) {
                edits.pop_front();
            }
            // Without every edit since, there is no knowing where these go.
            if version < current
                && edits
                    .front()
                    .is_none_or(|&(edited, _)| edited != version + 1)
            {
                return;
            }
            edits.iter().map(|&(_, shift)| shift).collect()
        };
        self.clear_diagnostics();
        let encoding = self.client.encoding();
        let table = self.buffer.tag_table();
        let mut shown = self.diagnostics.borrow_mut();
        for diagnostic in diagnostics {
            let color = match diagnostic.severity {
                None | Some(DiagnosticSeverity::ERROR) => ERROR,
                Some(DiagnosticSeverity::WARNING) => WARNING,
                Some(DiagnosticSeverity::INFORMATION) => INFORMATION,
                Some(_) => continue,
            };
            let range = shifts.iter().fold(diagnostic.range, |range, shift| {
                Range::new(shift.apply(range.start), shift.apply(range.end))
            });
            let mut start = encoding.iter_at(&self.buffer, range.start);
            let mut end = encoding.iter_at(&self.buffer, range.end);
            // Underline at least a character, where the line has one.
            if start == end {
                if !end.ends_line() {
                    end.forward_char();
                } else if start.line_offset() > 0 {
                    start.backward_char();
                }
            }
            let tag = gtk::TextTag::builder()
                .underline(pango::Underline::Error)
                .underline_rgba(&color)
                .build();
            table.add(&tag);
            self.buffer.apply_tag(&tag, &start, &end);
            shown.push((tag, diagnostic.message));
        }
    }

    fn clear_diagnostics(&self) {
        let table = self.buffer.tag_table();
        for (tag, _) in self.diagnostics.take() {
            table.remove(&tag);
        }
    }

    /// The messages of the diagnostics at `iter`.
    pub(super) fn diagnostics_at(&self, iter: &gtk::TextIter) -> Vec<String> {
        self.diagnostics
            .borrow()
            .iter()
            .filter(|(tag, _)| iter.has_tag(tag))
            .map(|(_, message)| message.clone())
            .collect()
    }

    /// The server's description of what is at `iter`, as plain text.
    pub(super) fn hover(
        &self,
        iter: &gtk::TextIter,
    ) -> impl Future<Output = Option<String>> + use<> {
        let request = self.position_request(iter, |capabilities| {
            capabilities
                .hover_provider
                .as_ref()
                .is_some_and(|provider| *provider != HoverProviderCapability::Simple(false))
        });
        async move {
            let (client, params) = request?;
            let hover = client
                .request::<HoverRequest>(HoverParams {
                    text_document_position_params: params,
                    work_done_progress_params: WorkDoneProgressParams::default(),
                })
                .await
                .ok()??;
            let text = match hover.contents {
                HoverContents::Scalar(text) => marked_text(text),
                HoverContents::Array(texts) => texts
                    .into_iter()
                    .map(marked_text)
                    .collect::<Vec<_>>()
                    .join("\n\n"),
                HoverContents::Markup(markup) => markup.value,
            };
            let text = tidy(&text);
            (!text.is_empty()).then_some(text)
        }
    }

    /// Whether typing `c` starts completion, as the server says.
    pub(super) fn is_completion_trigger(&self, c: char) -> bool {
        self.client.capabilities().is_some_and(|capabilities| {
            capabilities
                .completion_provider
                .as_ref()
                .and_then(|options| options.trigger_characters.as_ref())
                .is_some_and(|triggers| triggers.iter().any(|trigger| trigger.chars().eq([c])))
        })
    }

    /// The server's completions at `iter`, asked for in `context`.
    pub(super) fn complete(
        &self,
        iter: &gtk::TextIter,
        context: CompletionContext,
    ) -> impl Future<Output = Option<CompletionResponse>> + use<> {
        let request = self.position_request(iter, |capabilities| {
            capabilities.completion_provider.is_some()
        });
        async move {
            let (client, position) = request?;
            client
                .request::<Completion>(CompletionParams {
                    text_document_position: position,
                    work_done_progress_params: WorkDoneProgressParams::default(),
                    partial_result_params: PartialResultParams::default(),
                    context: Some(context),
                })
                .await
                .ok()?
        }
    }

    /// `item` as the server fills it in, if it may have left edits out, such
    /// as imports. Servers leave them out only of items they attach data to,
    /// to find them again, whether or not they declare they resolve items:
    /// rust-analyzer declares it only for other fields.
    pub(super) fn resolve(
        &self,
        item: &CompletionItem,
    ) -> Option<impl Future<Output = Option<CompletionItem>> + use<>> {
        let resolvable = item.additional_text_edits.is_none() && item.data.is_some();
        (resolvable && self.opened.get()).then(|| {
            let (client, item) = (Rc::clone(&self.client), item.clone());
            async move { client.request::<ResolveCompletionItem>(item).await.ok() }
        })
    }

    /// The file and position where what is at `iter` is defined.
    pub fn definition(
        &self,
        iter: &gtk::TextIter,
    ) -> impl Future<Output = Option<(gio::File, Position)>> + use<> {
        let request = self.position_request(iter, |capabilities| {
            capabilities
                .definition_provider
                .as_ref()
                .is_some_and(|provider| *provider != OneOf::Left(false))
        });
        async move {
            let (client, params) = request?;
            let response = client
                .request::<GotoDefinition>(GotoDefinitionParams {
                    text_document_position_params: params,
                    work_done_progress_params: WorkDoneProgressParams::default(),
                    partial_result_params: PartialResultParams::default(),
                })
                .await
                .ok()??;
            let (uri, position) = match response {
                GotoDefinitionResponse::Scalar(location) => (location.uri, location.range.start),
                GotoDefinitionResponse::Array(locations) => {
                    let location = locations.into_iter().next()?;
                    (location.uri, location.range.start)
                }
                GotoDefinitionResponse::Link(links) => {
                    let link = links.into_iter().next()?;
                    (link.target_uri, link.target_selection_range.start)
                }
            };
            Some((gio::File::for_uri(uri.as_str()), position))
        }
    }

    /// The client and the position of `iter`, for a request that the server
    /// takes if `takes` says so of its capabilities. The position is read now,
    /// as the buffer may change before the request is sent.
    fn position_request(
        &self,
        iter: &gtk::TextIter,
        takes: impl FnOnce(&ServerCapabilities) -> bool,
    ) -> Option<(Rc<Client>, TextDocumentPositionParams)> {
        let ready = self.opened.get()
            && self
                .client
                .capabilities()
                .is_some_and(|capabilities| takes(&capabilities));
        ready.then(|| {
            let position = self.client.encoding().position(iter);
            let document = TextDocumentIdentifier::new(self.uri.clone());
            (
                Rc::clone(&self.client),
                TextDocumentPositionParams::new(document, position),
            )
        })
    }
}

impl Drop for Document {
    fn drop(&mut self) {
        for handler in self.handlers.drain(..) {
            self.buffer.disconnect(handler);
        }
        self.view.hover().remove_provider(&self.hover);
        self.view.completion().remove_provider(&self.completion);
        self.clear_diagnostics();
        if self.opened.get() && self.client.capabilities().is_some() {
            self.client
                .notify::<DidCloseTextDocument>(DidCloseTextDocumentParams {
                    text_document: TextDocumentIdentifier::new(self.uri.clone()),
                });
        }
        self.client.remove(&self.uri);
    }
}

/// `text` without blank lines at either end or runs of them, which servers
/// use to pad sections.
fn tidy(text: &str) -> String {
    let mut lines: Vec<&str> = Vec::new();
    for line in text.lines().map(str::trim_end) {
        if !line.is_empty() || lines.last().is_some_and(|last| !last.is_empty()) {
            lines.push(line);
        }
    }
    if lines.last() == Some(&"") {
        lines.pop();
    }
    lines.join("\n")
}

fn marked_text(text: MarkedString) -> String {
    match text {
        MarkedString::String(text) => text,
        MarkedString::LanguageString(code) => code.value,
    }
}

#[cfg(test)]
mod tests {
    use super::tidy;

    #[test]
    fn tidying_keeps_single_blank_lines() {
        assert_eq!(tidy("\n  a  \n\n\n\nb\n\nc\n\n"), "  a\n\nb\n\nc");
        assert_eq!(tidy(" \n \n"), "");
    }
}
