//! Completion: the server's suggestions for the word at the cursor, narrowed
//! down and ranked as the word grows, and inserted with their snippets and
//! imports.

use std::cell::{Cell, RefCell};
use std::cmp::{Ordering, max};
use std::future::Future;
use std::iter::Peekable;
use std::pin::Pin;
use std::rc::{Rc, Weak};
use std::str::Chars;

use futures_util::FutureExt;
use futures_util::future::{LocalBoxFuture, Shared};
use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{gio, glib};
use lsp_types::{
    CompletionContext, CompletionItem, CompletionResponse, CompletionTextEdit,
    CompletionTriggerKind, Documentation, InsertTextFormat,
};
use sourceview::prelude::*;

use super::document::Document;

mod imp {
    use std::cell::{Cell, OnceCell, RefCell};
    use std::future::Future;
    use std::pin::Pin;
    use std::rc::{Rc, Weak};

    use gtk::subclass::prelude::*;
    use gtk::{gio, glib};
    use lsp_types::CompletionItem;
    use sourceview::subclass::prelude::*;

    use super::{Document, Resolution, Results};

    #[derive(Default)]
    pub struct Provider {
        pub(super) document: OnceCell<Weak<Document>>,
        /// The results shown, which typing narrows down. A request may still
        /// answer after later ones, for completion that has moved on.
        pub(super) results: RefCell<Vec<Rc<Results>>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for Provider {
        const NAME: &'static str = "CraftCompletionProvider";
        type Type = super::Provider;
        type Interfaces = (sourceview::CompletionProvider,);
    }

    impl ObjectImpl for Provider {}

    impl CompletionProviderImpl for Provider {
        fn is_trigger(&self, _iter: &gtk::TextIter, c: char) -> bool {
            self.obj()
                .document()
                .is_some_and(|document| document.is_completion_trigger(c))
        }

        fn populate_future(
            &self,
            context: &sourceview::CompletionContext,
        ) -> Pin<Box<dyn Future<Output = Result<gio::ListModel, glib::Error>>>> {
            self.obj().populate(context)
        }

        fn refilter(&self, context: &sourceview::CompletionContext, model: &gio::ListModel) {
            self.obj().refilter(context, model);
        }

        fn display(
            &self,
            context: &sourceview::CompletionContext,
            proposal: &sourceview::CompletionProposal,
            cell: &sourceview::CompletionCell,
        ) {
            self.obj().display(context, proposal, cell);
        }

        fn activate(
            &self,
            context: &sourceview::CompletionContext,
            proposal: &sourceview::CompletionProposal,
        ) {
            self.obj().activate(context, proposal);
        }
    }

    #[derive(Default)]
    pub struct Proposal {
        pub(super) item: OnceCell<CompletionItem>,
        /// How well the item matches the word typed, lower being better.
        pub(super) score: Cell<u32>,
        /// The item as the server fills it in, and the task asking for it.
        pub(super) resolution: OnceCell<(Resolution, glib::JoinHandle<()>)>,
    }

    impl Drop for Proposal {
        fn drop(&mut self) {
            // The server can drop the request too, unless an insertion awaits.
            if let Some((_, request)) = self.resolution.take() {
                request.abort();
            }
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for Proposal {
        const NAME: &'static str = "CraftCompletionProposal";
        type Type = super::Proposal;
        type Interfaces = (sourceview::CompletionProposal,);
    }

    impl ObjectImpl for Proposal {}
    impl CompletionProposalImpl for Proposal {}
}

/// A completion as the server fills it in, if it answers.
type Resolution = Shared<LocalBoxFuture<'static, Option<Rc<CompletionItem>>>>;

glib::wrapper! {
    pub struct Provider(ObjectSubclass<imp::Provider>)
        @implements sourceview::CompletionProvider;
}

glib::wrapper! {
    pub struct Proposal(ObjectSubclass<imp::Proposal>)
        @implements sourceview::CompletionProposal;
}

impl Provider {
    pub fn new(document: Weak<Document>) -> Self {
        let provider: Self = glib::Object::new();
        provider
            .imp()
            .document
            .set(document)
            .unwrap_or_else(|_| unreachable!("the document is set once"));
        provider
    }

    fn document(&self) -> Option<Rc<Document>> {
        self.imp().document.get().and_then(Weak::upgrade)
    }

    fn populate(
        &self,
        context: &sourceview::CompletionContext,
    ) -> Pin<Box<dyn Future<Output = Result<gio::ListModel, glib::Error>>>> {
        let request = self
            .document()
            .zip(context.bounds())
            .map(|(document, (_, cursor))| {
                // The character just typed, if the server takes it to start
                // completion.
                let mut previous = cursor;
                let trigger = previous
                    .backward_char()
                    .then(|| previous.char())
                    .filter(|&c| document.is_completion_trigger(c));
                document.complete(&cursor, trigger_context(trigger))
            });
        let (provider, context) = (self.downgrade(), context.clone());
        Box::pin(async move {
            let response = match request {
                Some(request) => request.await,
                None => None,
            };
            let (results, model) = Results::new(response, &context.word());
            if let Some(provider) = provider.upgrade() {
                let mut shown = provider.imp().results.borrow_mut();
                shown.retain(|results| results.model.upgrade().is_some());
                shown.push(results);
            }
            Ok(model.upcast())
        })
    }

    /// Narrows the results down to the word as it now is, asking the server
    /// again if it left items out.
    fn refilter(&self, context: &sourceview::CompletionContext, model: &gio::ListModel) {
        let Some(results) = self
            .imp()
            .results
            .borrow()
            .iter()
            .find(|results| {
                results
                    .model
                    .upgrade()
                    .is_some_and(|shown| shown.upcast_ref::<gio::ListModel>() == model)
            })
            .cloned()
        else {
            return;
        };
        results.refilter(&context.word());
        let (Some(document), Some((_, cursor))) = (self.document(), context.bounds()) else {
            return;
        };
        if results.incomplete.get() {
            let request = document.complete(
                &cursor,
                CompletionContext {
                    trigger_kind: CompletionTriggerKind::TRIGGER_FOR_INCOMPLETE_COMPLETIONS,
                    trigger_character: None,
                },
            );
            let (weak, context) = (Rc::downgrade(&results), context.clone());
            let request = glib::spawn_future_local(async move {
                let response = request.await;
                if let (Some(response), Some(results)) = (response, weak.upgrade()) {
                    results.replace(response);
                    results.refilter(&context.word());
                }
            });
            // Only the latest request counts: the server can drop the others.
            if let Some(previous) = results.request.replace(Some(request)) {
                previous.abort();
            }
        }
    }

    fn display(
        &self,
        context: &sourceview::CompletionContext,
        proposal: &sourceview::CompletionProposal,
        cell: &sourceview::CompletionCell,
    ) {
        let Some(proposal) = proposal.downcast_ref::<Proposal>() else {
            return;
        };
        let item = proposal.item();
        match cell.column() {
            sourceview::CompletionColumn::TypedText => {
                let word = glib::casefold(context.word());
                match sourceview::Completion::fuzzy_highlight(&item.label, &word) {
                    Some(attributes) => cell.set_text_with_attributes(&item.label, &attributes),
                    None => cell.set_text(Some(&item.label)),
                }
            }
            sourceview::CompletionColumn::After => cell.set_text(item.detail.as_deref()),
            sourceview::CompletionColumn::Comment => {
                // Only the selected proposal shows its comment: have the
                // server fill it in, so it is ready if chosen.
                if let Some(document) = self.document() {
                    proposal.resolve(&document);
                }
                cell.set_text(documentation(item).and_then(|text| text.lines().next()));
            }
            sourceview::CompletionColumn::Details => cell.set_text(documentation(item)),
            _ => cell.set_text(None),
        }
    }

    fn activate(
        &self,
        context: &sourceview::CompletionContext,
        proposal: &sourceview::CompletionProposal,
    ) {
        let (Some(document), Some(bounds), Some(proposal)) = (
            self.document(),
            context.bounds(),
            proposal.downcast_ref::<Proposal>(),
        ) else {
            return;
        };
        let item = proposal.item();
        let Some(resolution) = proposal.resolve(&document) else {
            insert(&document, item, bounds);
            return;
        };
        if let Some(resolved) = resolution.peek() {
            insert(&document, resolved.as_deref().unwrap_or(item), bounds);
            return;
        }
        // The server has yet to answer: insert the item once it has, in place
        // of the word, keeping text typed after it meanwhile.
        let buffer = document.buffer();
        let word = (
            buffer.create_mark(None, &bounds.0, true),
            buffer.create_mark(None, &bounds.1, true),
        );
        let (document, item) = (Rc::downgrade(&document), item.clone());
        glib::spawn_future_local(async move {
            let resolved = resolution.await;
            if let Some(document) = document.upgrade() {
                let buffer = document.buffer();
                let bounds = (buffer.iter_at_mark(&word.0), buffer.iter_at_mark(&word.1));
                buffer.delete_mark(&word.0);
                buffer.delete_mark(&word.1);
                insert(&document, resolved.as_deref().unwrap_or(&item), bounds);
            }
        });
    }
}

/// The context of a request made on typing `trigger`, or on asking.
fn trigger_context(trigger: Option<char>) -> CompletionContext {
    CompletionContext {
        trigger_kind: if trigger.is_some() {
            CompletionTriggerKind::TRIGGER_CHARACTER
        } else {
            CompletionTriggerKind::INVOKED
        },
        trigger_character: trigger.map(String::from),
    }
}

/// One request's completions, narrowed down and ranked as the word grows.
struct Results {
    /// Every item the server gave.
    store: gio::ListStore,
    /// The items matching the word, best first, while shown.
    model: glib::WeakRef<gtk::SortListModel>,
    filter: gtk::CustomFilter,
    sorter: gtk::CustomSorter,
    /// The word typed so far, casefolded.
    word: Rc<RefCell<String>>,
    /// Whether the server left items out, to ask again as the word grows.
    incomplete: Cell<bool>,
    /// The request asking again, while under way.
    request: RefCell<Option<glib::JoinHandle<()>>>,
}

impl Results {
    /// The results of `response`, with the model showing them.
    fn new(response: Option<CompletionResponse>, word: &str) -> (Rc<Self>, gtk::SortListModel) {
        let word = Rc::new(RefCell::new(String::from(glib::casefold(word))));
        let filter = gtk::CustomFilter::new(glib::clone!(
            #[strong]
            word,
            move |proposal| proposal_of(proposal).matches(&word.borrow())
        ));
        let sorter = gtk::CustomSorter::new(|a, b| proposal_of(a).rank(proposal_of(b)).into());
        let store = gio::ListStore::new::<Proposal>();
        let filtered = gtk::FilterListModel::new(Some(store.clone()), Some(filter.clone()));
        let model = gtk::SortListModel::new(Some(filtered), Some(sorter.clone()));
        let results = Rc::new(Self {
            model: model.downgrade(),
            store,
            filter,
            sorter,
            word,
            incomplete: Cell::new(false),
            request: RefCell::default(),
        });
        if let Some(response) = response {
            results.replace(response);
        }
        (results, model)
    }

    fn replace(&self, response: CompletionResponse) {
        let (items, incomplete) = match response {
            CompletionResponse::Array(items) => (items, false),
            CompletionResponse::List(list) => (list.items, list.is_incomplete),
        };
        let proposals: Vec<Proposal> = items.into_iter().map(Proposal::new).collect();
        self.store.splice(0, self.store.n_items(), &proposals);
        self.incomplete.set(incomplete);
    }

    /// Narrows the items down to those matching `word`, and ranks them.
    fn refilter(&self, word: &str) {
        let word: String = glib::casefold(word).into();
        let narrower = word.starts_with(self.word.borrow().as_str());
        self.word.replace(word);
        // Only a narrower word leaves the scores of the other items alone.
        self.filter.changed(if narrower {
            gtk::FilterChange::MoreStrict
        } else {
            gtk::FilterChange::Different
        });
        self.sorter.changed(gtk::SorterChange::Different);
    }
}

impl Drop for Results {
    fn drop(&mut self) {
        if let Some(request) = self.request.take() {
            request.abort();
        }
    }
}

impl Proposal {
    fn new(item: CompletionItem) -> Self {
        let proposal: Self = glib::Object::new();
        proposal
            .imp()
            .item
            .set(item)
            .unwrap_or_else(|_| unreachable!("the item is set once"));
        proposal
    }

    fn item(&self) -> &CompletionItem {
        self.imp().item.get().expect("proposals hold items")
    }

    /// The item as the server fills it in, asked for the first time, if the
    /// server may have left something out.
    fn resolve(&self, document: &Document) -> Option<Resolution> {
        let resolution = &self.imp().resolution;
        if resolution.get().is_none() {
            let request = document.resolve(self.item())?;
            let shared = async move { request.await.map(Rc::new) }
                .boxed_local()
                .shared();
            let task = glib::spawn_future_local(shared.clone().map(drop));
            resolution
                .set((shared, task))
                .unwrap_or_else(|_| unreachable!("the resolution is set once"));
        }
        resolution.get().map(|(shared, _)| shared.clone())
    }

    /// Whether the item matches `word`, as a fuzzy match, noting how well.
    fn matches(&self, word: &str) -> bool {
        let item = self.item();
        let text = item.filter_text.as_deref().unwrap_or(&item.label);
        let score = sourceview::Completion::fuzzy_match(Some(text), word);
        self.imp().score.set(score.unwrap_or(u32::MAX));
        score.is_some()
    }

    /// Better matches first, then in the server's order.
    fn rank(&self, other: &Self) -> Ordering {
        self.imp()
            .score
            .get()
            .cmp(&other.imp().score.get())
            .then_with(|| sort_key(self.item()).cmp(sort_key(other.item())))
    }
}

/// Where the server ranks `item` among its others.
fn sort_key(item: &CompletionItem) -> &str {
    item.sort_text.as_deref().unwrap_or(&item.label)
}

fn proposal_of(object: &glib::Object) -> &Proposal {
    object
        .downcast_ref()
        .expect("completion models hold proposals")
}

fn documentation(item: &CompletionItem) -> Option<&str> {
    let text = match item.documentation.as_ref()? {
        Documentation::String(text) => text,
        Documentation::MarkupContent(markup) => &markup.value,
    };
    Some(text.trim()).filter(|text| !text.is_empty())
}

/// Inserts `item` in place of the word between `bounds`, with the item's other
/// edits, such as imports, as one undoable action.
fn insert(document: &Document, item: &CompletionItem, bounds: (gtk::TextIter, gtk::TextIter)) {
    let (buffer, view, encoding) = (document.buffer(), document.view(), document.encoding());
    let (word, cursor) = bounds;
    // Typing may have gone on while the server filled the item in.
    let moved_on = buffer.iter_at_mark(&buffer.get_insert()) != cursor;
    // The server's edit is for the text it was asked about: it extends over
    // what has been typed since.
    let (start, end, text) = match &item.text_edit {
        Some(CompletionTextEdit::Edit(edit)) => (edit.range.start, edit.range.end, &edit.new_text),
        Some(CompletionTextEdit::InsertAndReplace(edit)) => {
            (edit.insert.start, edit.insert.end, &edit.new_text)
        }
        None => {
            let text = item.insert_text.as_ref().unwrap_or(&item.label);
            (encoding.position(&word), encoding.position(&cursor), text)
        }
    };
    let start = encoding.iter_at(buffer, start);
    let end = max(encoding.iter_at(buffer, end), cursor);

    buffer.begin_user_action();
    // Marks keep each edit's place while the others change the text.
    let main = marks(buffer, &start, &end);
    let edits: Vec<_> = item
        .additional_text_edits
        .iter()
        .flatten()
        .map(|edit| {
            let start = encoding.iter_at(buffer, edit.range.start);
            let end = encoding.iter_at(buffer, edit.range.end);
            (marks(buffer, &start, &end), edit.new_text.as_str())
        })
        .collect();
    // The word goes first, for undoing to come back to it, and the item's
    // text last, as a snippet takes any later edit for its own. What the
    // other edits insert where the word was goes before the item's text.
    let place = replace(buffer, &main.0, &main.1);
    let place = buffer.create_mark(None, &place, false);
    for ((start, end), text) in edits {
        let mut iter = replace(buffer, &start, &end);
        buffer.insert(&mut iter, text);
    }
    let mut iter = buffer.iter_at_mark(&place);
    buffer.delete_mark(&place);
    match item.insert_text_format {
        Some(InsertTextFormat::SNIPPET) => match sourceview::Snippet::new_parsed(text) {
            // Its fields are for filling in, unless typing has moved on.
            Ok(snippet) if !moved_on => view.push_snippet(&snippet, Some(&mut iter)),
            _ => buffer.insert(&mut iter, &snippet_text(text)),
        },
        _ => buffer.insert(&mut iter, text),
    }
    buffer.end_user_action();
}

fn marks(
    buffer: &sourceview::Buffer,
    start: &gtk::TextIter,
    end: &gtk::TextIter,
) -> (gtk::TextMark, gtk::TextMark) {
    (
        buffer.create_mark(None, start, true),
        buffer.create_mark(None, end, false),
    )
}

/// Deletes the text between two marks, and them, returning where it was.
fn replace(
    buffer: &sourceview::Buffer,
    start: &gtk::TextMark,
    end: &gtk::TextMark,
) -> gtk::TextIter {
    let (mut from, mut to) = (buffer.iter_at_mark(start), buffer.iter_at_mark(end));
    buffer.delete(&mut from, &mut to);
    buffer.delete_mark(start);
    buffer.delete_mark(end);
    from
}

/// The text a snippet inserts, without its tab stops: placeholders become
/// their default text, and choices their first option.
fn snippet_text(snippet: &str) -> String {
    let mut text = String::new();
    expand(&mut snippet.chars().peekable(), &mut text, false);
    text
}

fn expand(chars: &mut Peekable<Chars<'_>>, text: &mut String, nested: bool) {
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.peek() {
                Some(&next) if matches!(next, '$' | '}' | '\\' | ',' | '|') => {
                    text.push(next);
                    chars.next();
                }
                _ => text.push('\\'),
            },
            '$' => match chars.peek() {
                Some('{') => {
                    chars.next();
                    while chars
                        .next_if(|&c| c.is_alphanumeric() || c == '_')
                        .is_some()
                    {}
                    match chars.next() {
                        Some(':') => expand(chars, text, true),
                        Some('|') => choose(chars, text),
                        _ => {}
                    }
                }
                Some(&c) if c.is_alphanumeric() || c == '_' => {
                    while chars
                        .next_if(|&c| c.is_alphanumeric() || c == '_')
                        .is_some()
                    {}
                }
                _ => text.push('$'),
            },
            '}' if nested => return,
            c => text.push(c),
        }
    }
}

/// Keeps the first option of a choice, skipping the rest and its end.
fn choose(chars: &mut Peekable<Chars<'_>>, text: &mut String) {
    let mut first = true;
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                if let Some(next) = chars.next()
                    && first
                {
                    text.push(next);
                }
            }
            ',' => first = false,
            '|' => {
                chars.next_if_eq(&'}');
                return;
            }
            c if first => text.push(c),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::snippet_text;

    #[test]
    fn snippets_become_their_default_text() {
        assert_eq!(snippet_text("hello($0)"), "hello()");
        assert_eq!(
            snippet_text("call(${1:name}, ${2:count})$0"),
            "call(name, count)"
        );
        assert_eq!(snippet_text("${1:outer ${2:inner}}"), "outer inner");
        assert_eq!(snippet_text("${1|one,two|} \\$5 \\}"), "one $5 }");
        assert_eq!(snippet_text("$TM_FILENAME ${2} $"), "  $");
    }
}
