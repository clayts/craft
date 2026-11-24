//! An open document: a file and the source view editing it.

use std::borrow::Cow;

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::{gio, glib};
use sourceview::prelude::*;

use crate::display_name;

/// How many leading bytes of a file are used to guess its content type.
const SNIFF_LEN: usize = 8192;

mod imp {
    use std::cell::{Cell, OnceCell, RefCell};
    use std::marker::PhantomData;

    use adw::prelude::*;
    use adw::subclass::prelude::*;
    use gtk::{gio, glib};
    use sourceview::prelude::*;

    use crate::display_name;

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
            obj.set_child(Some(&gtk::ScrolledWindow::builder().child(view).build()));
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
        result
    }

    async fn write(&self) -> Result<(), glib::Error> {
        let imp = self.imp();
        let buffer = &imp.buffer;
        let generation = imp.generation.get();
        let text = buffer.text(&buffer.start_iter(), &buffer.end_iter(), true);
        let mut contents = if imp.crlf.get() {
            text.replace('\n', "\r\n").into()
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
