//! The hover popover's content: the diagnostics under the pointer and the
//! server's description of what is there, which can be scrolled through and
//! selected to copy.

use std::rc::Weak;
use std::time::Duration;

use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{glib, pango};

use super::document::Document;

/// How long to wait for the server's description before showing the rest.
const TIMEOUT: Duration = Duration::from_secs(2);

mod imp {
    use std::cell::OnceCell;
    use std::future::Future;
    use std::pin::Pin;
    use std::rc::Weak;

    use gtk::glib;
    use gtk::prelude::*;
    use gtk::subclass::prelude::*;
    use sourceview::subclass::prelude::*;

    use super::{Document, TIMEOUT, content, label};

    #[derive(Default)]
    pub struct HoverProvider {
        pub(super) document: OnceCell<Weak<Document>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for HoverProvider {
        const NAME: &'static str = "CraftHoverProvider";
        type Type = super::HoverProvider;
        type Interfaces = (sourceview::HoverProvider,);
    }

    impl ObjectImpl for HoverProvider {}

    impl HoverProviderImpl for HoverProvider {
        fn populate_future(
            &self,
            context: &sourceview::HoverContext,
            display: &sourceview::HoverDisplay,
        ) -> Pin<Box<dyn Future<Output = Result<(), glib::Error>>>> {
            let document = self.document.get().and_then(Weak::upgrade);
            let display = display.clone();
            let view = context.view();
            // Read the buffer now: it may change before the future runs.
            let (messages, description) = match (document, context.iter()) {
                (Some(document), Some(iter)) => {
                    (document.diagnostics_at(&iter), Some(document.hover(&iter)))
                }
                _ => (Vec::new(), None),
            };
            Box::pin(async move {
                let parts = gtk::Box::new(gtk::Orientation::Vertical, 0);
                for message in &messages {
                    parts.append(&label(message, false));
                }
                if let Some(description) = description
                    && let Ok(description) = glib::future_with_timeout(TIMEOUT, description).await
                {
                    for part in description {
                        if parts.first_child().is_some() {
                            parts.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
                        }
                        parts.append(&label(&part, true));
                    }
                }
                if parts.first_child().is_some() {
                    display.append(&content(&view, &parts));
                }
                Ok(())
            })
        }
    }
}

glib::wrapper! {
    pub struct HoverProvider(ObjectSubclass<imp::HoverProvider>)
        @implements sourceview::HoverProvider;
}

impl HoverProvider {
    pub fn new(document: Weak<Document>) -> Self {
        let provider: Self = glib::Object::new();
        provider
            .imp()
            .document
            .set(document)
            .unwrap_or_else(|_| unreachable!("the document is set once"));
        provider
    }
}

/// `parts` as the popover shows them: scrolling once they take two fifths of
/// `view`'s height, so that the popover fits above or below what it describes,
/// as one that fits on neither side is not shown at all. Selecting text gives
/// them the focus, to copy it, and the view takes it back once they close.
fn content(view: &sourceview::View, parts: &gtk::Box) -> gtk::ScrolledWindow {
    let content = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .propagate_natural_width(true)
        .propagate_natural_height(true)
        .max_content_height(view.height() * 2 / 5)
        .child(parts)
        .build();
    let view = view.downgrade();
    content.connect_unmap(move |content| {
        let focused = (content.root().and_then(|root| root.focus()))
            .is_some_and(|focus| focus.is_ancestor(content));
        if focused && let Some(view) = view.upgrade() {
            view.grab_focus();
        }
    });
    content
}

/// A label showing `text`, which is Pango markup if `markup` says so.
fn label(text: &str, markup: bool) -> gtk::Label {
    gtk::Label::builder()
        .label(text)
        .use_markup(markup)
        .selectable(true)
        .wrap(true)
        .wrap_mode(pango::WrapMode::WordChar)
        .max_width_chars(80)
        .xalign(0.0)
        .margin_start(8)
        .margin_end(8)
        .margin_top(6)
        .margin_bottom(6)
        .build()
}
