//! The hover popover's content: the diagnostics under the pointer and the
//! server's description of what is there.

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
    use gtk::subclass::prelude::*;
    use sourceview::subclass::prelude::*;

    use super::{Document, TIMEOUT, label};

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
            // Read the buffer now: it may change before the future runs.
            let (messages, description) = match (document, context.iter()) {
                (Some(document), Some(iter)) => {
                    (document.diagnostics_at(&iter), Some(document.hover(&iter)))
                }
                _ => (Vec::new(), None),
            };
            Box::pin(async move {
                let shown = !messages.is_empty();
                for message in messages {
                    display.append(&label(&message, false));
                }
                if let Some(description) = description
                    && let Ok(Some(text)) = glib::future_with_timeout(TIMEOUT, description).await
                {
                    if shown {
                        display.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
                    }
                    display.append(&label(&text, true));
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

fn label(text: &str, monospace: bool) -> gtk::Label {
    let label = gtk::Label::builder()
        .label(text)
        .wrap(true)
        .wrap_mode(pango::WrapMode::WordChar)
        .max_width_chars(80)
        .xalign(0.0)
        .margin_start(8)
        .margin_end(8)
        .margin_top(6)
        .margin_bottom(6)
        .build();
    if monospace {
        label.add_css_class("monospace");
    }
    label
}
