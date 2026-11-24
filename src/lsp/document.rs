//! An open file as its language server sees it: kept in sync with the buffer
//! and marked with the server's diagnostics.

use std::cell::{Cell, RefCell};
use std::future::Future;
use std::rc::{Rc, Weak};

use gtk::prelude::*;
use gtk::{gdk, gio, glib, pango};
use lsp_types::notification::{
    DidChangeTextDocument, DidCloseTextDocument, DidOpenTextDocument, DidSaveTextDocument,
};
use lsp_types::request::{GotoDefinition, HoverRequest};
use lsp_types::{
    Diagnostic, DiagnosticSeverity, DidChangeTextDocumentParams, DidCloseTextDocumentParams,
    DidOpenTextDocumentParams, DidSaveTextDocumentParams, GotoDefinitionParams,
    GotoDefinitionResponse, HoverContents, HoverParams, HoverProviderCapability, MarkedString,
    OneOf, PartialResultParams, Position, Range, TextDocumentContentChangeEvent,
    TextDocumentIdentifier, TextDocumentItem, TextDocumentPositionParams,
    TextDocumentSyncCapability, TextDocumentSyncKind, TextDocumentSyncOptions,
    TextDocumentSyncSaveOptions, Uri, VersionedTextDocumentIdentifier, WorkDoneProgressParams,
};
use sourceview::prelude::*;

use super::Encoding;
use super::client::Client;
use super::hover::HoverProvider;
use crate::editor::Editor;

const ERROR: gdk::RGBA = gdk::RGBA::new(0.878, 0.106, 0.141, 1.0);
const WARNING: gdk::RGBA = gdk::RGBA::new(0.898, 0.647, 0.039, 1.0);
const INFORMATION: gdk::RGBA = gdk::RGBA::new(0.208, 0.518, 0.894, 1.0);

pub struct Document {
    client: Rc<Client>,
    editor: glib::WeakRef<Editor>,
    uri: Uri,
    language: &'static str,
    buffer: sourceview::Buffer,
    view: sourceview::View,
    hover: HoverProvider,
    handlers: Vec<glib::SignalHandlerId>,
    /// Whether the server has the document, which it lacks while starting.
    opened: Cell<bool>,
    version: Cell<i32>,
    /// Each diagnostic's underline, which follows edits, and message.
    diagnostics: RefCell<Vec<(gtk::TextTag, String)>>,
}

impl Document {
    /// Connects `editor`, whose file is in the LSP language `language`, to
    /// `client`.
    pub fn new(client: &Rc<Client>, editor: &Editor, language: &'static str) -> Rc<Self> {
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
                handlers,
                opened: Cell::new(false),
                version: Cell::new(0),
                diagnostics: RefCell::default(),
            }
        });
        view.hover().add_provider(&document.hover);
        client.add(&document.uri, &document);
        document
    }

    pub fn editor(&self) -> Option<Editor> {
        self.editor.upgrade()
    }

    pub fn language(&self) -> &'static str {
        self.language
    }

    pub fn encoding(&self) -> Encoding {
        self.client.encoding()
    }

    /// Gives the server the document, as it now is.
    pub fn open(&self) {
        self.opened.set(true);
        self.version.set(0);
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
    pub fn closed(&self) {
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
            self.change(TextDocumentContentChangeEvent {
                range: Some(range),
                range_length: None,
                text: text.to_owned(),
            });
        }
    }

    /// Sends the text after an edit, for servers that take whole texts.
    fn replaced(&self) {
        if self.opened.get() && self.sync_kind() == Some(TextDocumentSyncKind::FULL) {
            self.change(TextDocumentContentChangeEvent {
                range: None,
                range_length: None,
                text: self.text(),
            });
        }
    }

    fn change(&self, change: TextDocumentContentChangeEvent) {
        let version = self.version.get() + 1;
        self.version.set(version);
        self.client
            .notify::<DidChangeTextDocument>(DidChangeTextDocumentParams {
                text_document: VersionedTextDocumentIdentifier::new(self.uri.clone(), version),
                content_changes: vec![change],
            });
    }

    fn text(&self) -> String {
        let (start, end) = self.buffer.bounds();
        self.buffer.text(&start, &end, true).into()
    }

    /// Underlines `diagnostics`, replacing the previous ones, unless they are
    /// for an older version of the text.
    pub fn set_diagnostics(&self, version: Option<i32>, diagnostics: Vec<Diagnostic>) {
        if !self.opened.get() || version.is_some_and(|version| version != self.version.get()) {
            return;
        }
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
            let mut start = encoding.iter_at(&self.buffer, diagnostic.range.start);
            let mut end = encoding.iter_at(&self.buffer, diagnostic.range.end);
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
    pub fn diagnostics_at(&self, iter: &gtk::TextIter) -> Vec<String> {
        self.diagnostics
            .borrow()
            .iter()
            .filter(|(tag, _)| iter.has_tag(tag))
            .map(|(_, message)| message.clone())
            .collect()
    }

    /// The server's description of what is at `iter`, as plain text.
    pub fn hover(&self, iter: &gtk::TextIter) -> impl Future<Output = Option<String>> + use<> {
        let supported = self.client.capabilities().is_some_and(|capabilities| {
            capabilities
                .hover_provider
                .as_ref()
                .is_some_and(|provider| *provider != HoverProviderCapability::Simple(false))
        });
        let request = (supported && self.opened.get())
            .then(|| (Rc::clone(&self.client), self.position_params(iter)));
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

    /// The file and position where what is at `iter` is defined.
    pub fn definition(
        &self,
        iter: &gtk::TextIter,
    ) -> impl Future<Output = Option<(gio::File, Position)>> + use<> {
        let supported = self.client.capabilities().is_some_and(|capabilities| {
            capabilities
                .definition_provider
                .as_ref()
                .is_some_and(|provider| *provider != OneOf::Left(false))
        });
        let request = (supported && self.opened.get())
            .then(|| (Rc::clone(&self.client), self.position_params(iter)));
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

    fn position_params(&self, iter: &gtk::TextIter) -> TextDocumentPositionParams {
        TextDocumentPositionParams::new(
            TextDocumentIdentifier::new(self.uri.clone()),
            self.client.encoding().position(iter),
        )
    }
}

impl Drop for Document {
    fn drop(&mut self) {
        for handler in self.handlers.drain(..) {
            self.buffer.disconnect(handler);
        }
        self.view.hover().remove_provider(&self.hover);
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
