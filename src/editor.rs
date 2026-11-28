//! An open document: a file and the source view editing it.

use std::borrow::Cow;
use std::rc::Rc;

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::{gdk, gio, glib};
use sourceview::prelude::*;

use crate::display_name;
use crate::lsp::{Document, Encoding};

/// How many leading bytes of a file are used to guess its content type.
const SNIFF_LEN: usize = 8192;

mod imp {
    use std::cell::{Cell, OnceCell, RefCell};
    use std::marker::PhantomData;
    use std::rc::Rc;

    use adw::prelude::*;
    use adw::subclass::prelude::*;
    use gtk::{gio, glib};
    use sourceview::prelude::*;

    use crate::display_name;
    use crate::hover::Hover;
    use crate::lsp::Document;

    #[derive(Default, glib::Properties)]
    #[properties(wrapper_type = super::Editor)]
    pub struct Editor {
        #[property(get, construct_only)]
        file: OnceCell<gio::File>,
        /// The file's name, marked while it has unsaved changes.
        #[property(get = Self::title, type = String)]
        title: PhantomData<String>,
        pub(super) buffer: sourceview::Buffer,
        pub(super) view: sourceview::View,
        /// The file's entity tag as last read or written, which reveals
        /// changes made by other programs.
        pub(super) etag: RefCell<Option<glib::GString>>,
        /// Whether the file's lines end in `\r\n`. The buffer holds `\n`, so
        /// new lines match old ones, and saving restores `\r\n`.
        pub(super) crlf: Cell<bool>,
        /// Counts buffer changes, so a save can tell whether the buffer still
        /// holds what it wrote.
        pub(super) generation: Cell<u64>,
        pub(super) saving: Cell<bool>,
        /// The file as its language server has it, if one is running.
        pub(super) document: RefCell<Option<Rc<Document>>>,
        hover: OnceCell<Hover>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for Editor {
        const NAME: &'static str = "CraftEditor";
        type Type = super::Editor;
        type ParentType = adw::Bin;
    }

    #[glib::derived_properties]
    impl ObjectImpl for Editor {
        fn constructed(&self) {
            self.parent_constructed();
            let obj = self.obj();

            self.buffer.connect_changed(glib::clone!(
                #[weak]
                obj,
                move |_| {
                    let generation = &obj.imp().generation;
                    generation.set(generation.get().wrapping_add(1));
                }
            ));
            self.buffer.connect_modified_changed(glib::clone!(
                #[weak]
                obj,
                move |_| obj.notify_title()
            ));
            adw::StyleManager::default()
                .bind_property("dark", &self.buffer, "style-scheme")
                .transform_to(|_, dark: bool| {
                    sourceview::StyleSchemeManager::default().scheme(if dark {
                        "Adwaita-dark"
                    } else {
                        "Adwaita"
                    })
                })
                .sync_create()
                .build();

            let view = &self.view;
            view.set_buffer(Some(&self.buffer));
            view.set_monospace(true);
            view.set_show_line_numbers(true);
            view.set_highlight_current_line(true);
            view.set_auto_indent(true);
            view.set_smart_backspace(true);
            view.set_smart_home_end(sourceview::SmartHomeEndType::Before);
            view.set_tab_width(4);
            // Tab moves through the fields of a completion's snippet.
            view.set_enable_snippets(true);
            // Enter or Tab takes the best completion.
            let completion = view.completion();
            completion.set_show_icons(false);
            completion.set_select_on_show(true);
            // Right-clicking text opens its menu, Ctrl+clicking it goes to
            // its definition.
            let click = gtk::GestureClick::builder()
                .button(0)
                .propagation_phase(gtk::PropagationPhase::Capture)
                .build();
            click.connect_pressed(glib::clone!(
                #[weak]
                view,
                move |click, _, x, y| super::clicked(&view, click, x, y)
            ));
            view.add_controller(click);
            let definition =
                gio::MenuItem::new(Some("Go to _Definition"), Some("win.go-to-definition"));
            definition.set_attribute_value("accel", Some(&"F12".to_variant()));
            let section = gio::Menu::new();
            section.append_item(&definition);
            let menu = gio::Menu::new();
            menu.append_section(None, &section);
            view.set_extra_menu(Some(&menu));
            obj.set_child(Some(&gtk::ScrolledWindow::builder().child(view).build()));
            // Once the view is in its scrolled window, whose scrolling the
            // hover watches.
            self.hover
                .set(Hover::new(&obj))
                .unwrap_or_else(|_| unreachable!("the hover is set once"));
        }

        fn dispose(&self) {
            // While the view is intact for the document to detach from it.
            self.document.take();
            if let Some(hover) = self.hover.get() {
                hover.unparent();
            }
        }
    }

    impl WidgetImpl for Editor {
        fn grab_focus(&self) -> bool {
            self.view.grab_focus()
        }
    }

    impl BinImpl for Editor {}

    impl Editor {
        fn title(&self) -> String {
            let name = display_name(&self.obj().file());
            if self.buffer.is_modified() {
                format!("• {name}")
            } else {
                name
            }
        }
    }
}

glib::wrapper! {
    pub struct Editor(ObjectSubclass<imp::Editor>)
        @extends adw::Bin, gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl Editor {
    /// Reads `file` into a new editor. A file that does not exist yet opens
    /// empty and is created on save.
    pub async fn open(file: gio::File) -> Result<Self, glib::Error> {
        match file.load_contents_future().await {
            Ok((bytes, etag)) => match std::str::from_utf8(&bytes) {
                Ok(text) if !text.contains('\0') => Ok(Self::new(file, text, etag)),
                _ => Err(glib::Error::new(
                    gio::IOErrorEnum::InvalidData,
                    "Not a UTF-8 text file",
                )),
            },
            Err(err) if err.matches(gio::IOErrorEnum::NotFound) => Ok(Self::new(file, "", None)),
            Err(err) => Err(err),
        }
    }

    fn new(file: gio::File, text: &str, etag: Option<glib::GString>) -> Self {
        let name = file.basename();
        let editor: Self = glib::Object::builder().property("file", file).build();
        let imp = editor.imp();

        let sniff = &text.as_bytes()[..text.len().min(SNIFF_LEN)];
        let (content_type, _) = gio::content_type_guess(name.as_ref(), sniff);
        let language = sourceview::LanguageManager::default()
            .guess_language(name.as_ref(), Some(&content_type));
        // Languages whose syntax or universal convention is tab indentation.
        let tabs = language
            .as_ref()
            .is_some_and(|language| matches!(language.id().as_str(), "go" | "makefile"));
        imp.view.set_insert_spaces_instead_of_tabs(!tabs);

        // The first line ending decides the file's style.
        let crlf = text
            .find('\n')
            .is_some_and(|end| text[..end].ends_with('\r'));
        let text = if crlf {
            Cow::Owned(text.replace("\r\n", "\n"))
        } else {
            Cow::Borrowed(text)
        };
        imp.crlf.set(crlf);

        let buffer = &imp.buffer;
        buffer.set_language(language.as_ref());
        buffer.begin_irreversible_action();
        buffer.set_text(&text);
        buffer.end_irreversible_action();
        buffer.set_modified(false);
        buffer.place_cursor(&buffer.start_iter());
        imp.etag.replace(etag);
        editor
    }

    pub fn is_modified(&self) -> bool {
        self.imp().buffer.is_modified()
    }

    pub fn buffer(&self) -> sourceview::Buffer {
        self.imp().buffer.clone()
    }

    pub fn view(&self) -> sourceview::View {
        self.imp().view.clone()
    }

    /// The identifier of the file's `GtkSourceView` language.
    pub fn language(&self) -> Option<glib::GString> {
        self.imp().buffer.language().map(|language| language.id())
    }

    pub fn document(&self) -> Option<Rc<Document>> {
        self.imp().document.borrow().clone()
    }

    pub fn set_document(&self, document: Option<Rc<Document>>) {
        self.imp().document.replace(document);
    }

    /// Moves the cursor to `position`, which the language server gave in
    /// `encoding`, and scrolls it into view.
    pub fn goto(&self, position: lsp_types::Position, encoding: Encoding) {
        let iter = encoding.iter_at(&self.imp().buffer, position);
        self.reveal(&iter, &iter);
    }

    /// Selects from `start` to `end`, with the cursor at `start`, and scrolls
    /// the cursor into view.
    pub fn reveal(&self, start: &gtk::TextIter, end: &gtk::TextIter) {
        let imp = self.imp();
        imp.buffer.select_range(start, end);
        // GTK scrolls in an animation aimed where the cursor is as it
        // begins, which falls short while the lines above are still being
        // measured, as in a new tab. So the view first gets its size, then
        // its lines measured, which GTK does at a higher priority.
        imp.view.add_tick_callback(|view, _| {
            if view.height() == 0 {
                return glib::ControlFlow::Continue;
            }
            let view = view.downgrade();
            glib::idle_add_local_full(glib::Priority::LOW, move || {
                if let Some(view) = view.upgrade() {
                    scroll_to_cursor(&view);
                }
                glib::ControlFlow::Break
            });
            glib::ControlFlow::Break
        });
        self.grab_focus();
    }

    /// Writes the buffer to the file in the file's line ending style. If
    /// another program has changed the file since it was read, asks before
    /// overwriting; declining fails with [`gio::IOErrorEnum::Cancelled`].
    /// Does nothing while a save is already in progress.
    pub async fn save(&self) -> Result<(), glib::Error> {
        let imp = self.imp();
        if imp.saving.replace(true) {
            return Ok(());
        }
        let result = self.write().await;
        imp.saving.set(false);
        if result.is_ok()
            && let Some(document) = self.document()
        {
            document.saved();
        }
        result
    }

    async fn write(&self) -> Result<(), glib::Error> {
        let imp = self.imp();
        let buffer = &imp.buffer;
        let generation = imp.generation.get();
        let text = buffer.text(&buffer.start_iter(), &buffer.end_iter(), true);
        let mut contents = if imp.crlf.get() {
            with_crlf(&text).into()
        } else {
            text
        };
        let mut etag = imp.etag.borrow().clone();
        loop {
            match self
                .file()
                .replace_contents_future(
                    contents,
                    etag.as_deref(),
                    false,
                    gio::FileCreateFlags::NONE,
                )
                .await
            {
                Ok((_, new_etag)) => {
                    imp.etag.replace(new_etag);
                    // Edits made while writing are still unsaved.
                    if imp.generation.get() == generation {
                        buffer.set_modified(false);
                    }
                    return Ok(());
                }
                Err((returned, err)) if err.matches(gio::IOErrorEnum::WrongEtag) => {
                    if !self.confirm_overwrite().await {
                        return Err(glib::Error::new(
                            gio::IOErrorEnum::Cancelled,
                            "Save cancelled",
                        ));
                    }
                    contents = returned;
                    etag = None;
                }
                Err((_, err)) => return Err(err),
            }
        }
    }

    async fn confirm_overwrite(&self) -> bool {
        let dialog = adw::AlertDialog::new(
            Some("File Changed on Disk"),
            Some(&format!(
                "“{}” was changed by another program. Saving will discard those changes.",
                display_name(&self.file())
            )),
        );
        dialog.add_responses(&[("cancel", "_Cancel"), ("overwrite", "_Overwrite")]);
        dialog.set_response_appearance("overwrite", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");
        dialog.choose_future(Some(self)).await == "overwrite"
    }
}

/// Acts on a click at `x`, `y` in `view` on text: a right-click moves the
/// cursor to it, unless it is selected, for the menu to act on; a Ctrl+click
/// moves the cursor to it and goes to its definition, in place of what a
/// click does.
fn clicked(view: &sourceview::View, click: &gtk::GestureClick, x: f64, y: f64) {
    let button = click.current_button();
    let definition = button == gdk::BUTTON_PRIMARY
        && (click.current_event_state()).contains(gdk::ModifierType::CONTROL_MASK);
    if button != gdk::BUTTON_SECONDARY && !definition {
        return;
    }
    #[expect(clippy::cast_possible_truncation, reason = "pixels fit")]
    let (x, y) = view.window_to_buffer_coords(gtk::TextWindowType::Widget, x as i32, y as i32);
    let Some(iter) = view.iter_at_location(x, y) else {
        return;
    };
    let buffer = view.buffer();
    if definition {
        buffer.place_cursor(&iter);
        click.set_state(gtk::EventSequenceState::Claimed);
        view.completion().hide();
        view.grab_focus();
        view.activate_action("win.go-to-definition", None)
            .expect("views clicked in are in windows, which have it");
    } else if !(buffer.selection_bounds()).is_some_and(|(start, end)| iter.in_range(&start, &end)) {
        buffer.place_cursor(&iter);
    }
}

/// Scrolls `view` to show the cursor a third of the way down, and as far
/// left as it can be while still showing it.
fn scroll_to_cursor(view: &sourceview::View) {
    let insert = view.buffer().get_insert();
    let cursor = view.iter_location(&view.buffer().iter_at_mark(&insert));
    let width = view.visible_rect().width();
    // Aligning down aligns across too, here to the left edge, or else as far
    // right as the cursor is beyond the screen's width.
    let across = f64::from(cursor.x()) / f64::from((width - cursor.width()).max(1));
    view.scroll_to_mark(&insert, 0.0, true, across.min(1.0), 0.3);
}

/// `text` with every line ended by `\r\n`, including lines pasted in with
/// that ending already.
fn with_crlf(text: &str) -> String {
    let mut result = String::with_capacity(text.len() + text.matches('\n').count());
    let mut rest = text;
    while let Some(end) = rest.find('\n') {
        let line = &rest[..end];
        result.push_str(line.strip_suffix('\r').unwrap_or(line));
        result.push_str("\r\n");
        rest = &rest[end + 1..];
    }
    result.push_str(rest);
    result
}

#[cfg(test)]
mod tests {
    use super::with_crlf;

    #[test]
    fn crlf_endings_are_not_doubled() {
        assert_eq!(with_crlf("a\nb\r\nc"), "a\r\nb\r\nc");
        assert_eq!(with_crlf("a\n\n"), "a\r\n\r\n");
        assert_eq!(with_crlf("a\r"), "a\r");
        assert_eq!(with_crlf(""), "");
    }
}
