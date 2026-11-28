//! Finding and replacing across the project: a page of the sidebar that lists
//! the matches of what is typed into it in the project's files, but for those
//! that `.gitignore` files leave out, opens each on clicking it, and replaces
//! them all.

mod scan;

use std::collections::HashMap;
use std::path::PathBuf;

use adw::prelude::*;
use adw::subclass::prelude::*;
use futures_util::StreamExt;
use gtk::{gio, glib, pango};
use sourceview::prelude::*;

use crate::search::{options, show_error};
use crate::window::Window;
use scan::{Found, Hit, Query};

mod imp {
    use std::cell::{Cell, OnceCell, RefCell};

    use adw::subclass::prelude::*;
    use gtk::{gio, glib};

    #[derive(Default)]
    pub struct ProjectSearch {
        pub(super) root: RefCell<Option<gio::File>>,
        /// Shows the page in place of the file tree while active.
        pub(super) button: gtk::ToggleButton,
        pub(super) entry: gtk::SearchEntry,
        pub(super) replacement: gtk::Entry,
        pub(super) replace: gtk::Button,
        /// How many matches there are, and in how many files.
        pub(super) status: gtk::Label,
        /// What to find and how.
        pub(super) settings: sourceview::SearchSettings,
        /// The files with matches, each followed by its matches.
        pub(super) results: OnceCell<gio::ListStore>,
        /// How many matches and files are listed.
        pub(super) counts: Cell<(usize, usize)>,
        /// The search under way, to cancel.
        pub(super) scan: RefCell<Option<(gio::Cancellable, glib::JoinHandle<()>)>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for ProjectSearch {
        const NAME: &'static str = "CraftProjectSearch";
        type Type = super::ProjectSearch;
        type ParentType = adw::Bin;
    }

    impl ObjectImpl for ProjectSearch {
        fn constructed(&self) {
            self.parent_constructed();
            self.obj().set_up();
        }

        fn dispose(&self) {
            self.obj().stop();
        }
    }

    impl WidgetImpl for ProjectSearch {}
    impl BinImpl for ProjectSearch {}
}

glib::wrapper! {
    pub struct ProjectSearch(ObjectSubclass<imp::ProjectSearch>)
        @extends adw::Bin, gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl Default for ProjectSearch {
    fn default() -> Self {
        glib::Object::new()
    }
}

/// A row of the results: a file, and how many matches it has, or a match.
enum Row {
    File { file: gio::File, count: usize },
    Hit { file: gio::File, hit: Hit },
}

impl ProjectSearch {
    /// The button that shows the page in place of the file tree.
    pub fn button(&self) -> &gtk::ToggleButton {
        &self.imp().button
    }

    /// Searches the files under `root`.
    pub fn set_root(&self, root: &gio::File) {
        self.imp().root.replace(Some(root.clone()));
        self.imp().button.set_visible(true);
        self.rescan();
    }

    /// Shows the page to find `text`, if given, or else what it had.
    pub fn open(&self, text: Option<&str>) {
        let imp = self.imp();
        imp.button.set_active(true);
        if let Some(text) = text {
            imp.entry.set_text(&if imp.settings.is_regex_enabled() {
                glib::Regex::escape_string(text)
            } else {
                text.into()
            });
        }
        imp.entry.grab_focus();
        imp.entry.select_region(0, -1);
    }

    fn set_up(&self) {
        let imp = self.imp();
        imp.button.set_icon_name("system-search-symbolic");
        imp.button.set_tooltip_text(Some("Find in Project"));
        imp.button.set_visible(false);
        imp.button.connect_active_notify(glib::clone!(
            #[weak(rename_to = search)]
            self,
            move |button| {
                // The editor takes the focus back from the page as it goes.
                if !button.is_active()
                    && search.focus_child().is_some()
                    && let Some(editor) = search.window().and_then(|window| window.current_editor())
                {
                    editor.grab_focus();
                }
            }
        ));

        let entry = &imp.entry;
        entry.set_placeholder_text(Some("Find in Project"));
        entry.connect_search_changed(glib::clone!(
            #[weak(rename_to = search)]
            self,
            move |entry| search.imp().settings.set_search_text(Some(&entry.text()))
        ));
        entry.connect_activate(calls(self, Self::show_first));
        entry.connect_stop_search(calls(self, Self::close));
        let rescan = calls(self, Self::rescan);
        imp.settings
            .connect_notify_local(None, move |settings, _| rescan(settings));

        imp.replacement.set_placeholder_text(Some("Replace"));
        imp.replacement.set_hexpand(true);
        imp.replacement.connect_changed(calls(self, Self::refresh));
        imp.replace.set_label("Replace _All");
        imp.replace.set_use_underline(true);
        imp.replace.connect_clicked(calls(self, Self::replace_all));
        let replacing = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        replacing.append(&imp.replacement);
        replacing.append(&imp.replace);

        let status = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        for option in options(&imp.settings) {
            status.append(&option);
        }
        imp.status.set_hexpand(true);
        imp.status.set_xalign(1.0);
        imp.status.set_ellipsize(pango::EllipsizeMode::End);
        imp.status.add_css_class("dim-label");
        status.append(&imp.status);

        let fields = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(6)
            .margin_start(6)
            .margin_end(6)
            .margin_bottom(6)
            .build();
        fields.append(entry);
        fields.append(&replacing);
        fields.append(&status);
        let page = gtk::Box::new(gtk::Orientation::Vertical, 0);
        page.append(&fields);
        page.append(&self.results_view());
        self.set_child(Some(&page));
        // Escape leaves the page from anywhere in it.
        let close = calls(self, Self::close);
        let escape = gtk::ShortcutController::new();
        escape.add_shortcut(gtk::Shortcut::new(
            gtk::ShortcutTrigger::parse_string("Escape"),
            Some(gtk::CallbackAction::new(move |widget, _| {
                close(widget);
                glib::Propagation::Stop
            })),
        ));
        self.add_controller(escape);
    }

    /// Shows the file tree in place of the page.
    fn close(&self) {
        self.imp().button.set_active(false);
    }

    /// The list of results, which shows a match on clicking it.
    fn results_view(&self) -> gtk::ScrolledWindow {
        let results = gio::ListStore::new::<glib::BoxedAnyObject>();
        self.imp()
            .results
            .set(results.clone())
            .unwrap_or_else(|_| unreachable!("the results are set once"));
        let factory = gtk::SignalListItemFactory::new();
        factory.connect_setup(|_, item| {
            let line = gtk::Label::builder()
                .width_chars(4)
                .xalign(1.0)
                .css_classes(["dim-label", "numeric"])
                .build();
            let text = gtk::Label::builder()
                .hexpand(true)
                .xalign(0.0)
                .ellipsize(pango::EllipsizeMode::End)
                .build();
            let count = gtk::Label::builder()
                .css_classes(["dim-label", "numeric"])
                .build();
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            row.append(&line);
            row.append(&text);
            row.append(&count);
            list_item(item).set_child(Some(&row));
        });
        factory.connect_bind(glib::clone!(
            #[weak(rename_to = search)]
            self,
            move |_, item| {
                let item = list_item(item);
                let label = |widget: Option<gtk::Widget>| -> gtk::Label {
                    widget.and_downcast().expect("rows hold labels")
                };
                let row = item.child().expect("rows are set up");
                let line = label(row.first_child());
                let text = label(line.next_sibling());
                let count = label(row.last_child());
                let object = item.item().and_downcast::<glib::BoxedAnyObject>();
                match &*object.expect("results hold rows").borrow::<Row>() {
                    Row::File {
                        file,
                        count: matches,
                    } => {
                        line.set_visible(false);
                        text.set_markup(&heading(search.imp().root.borrow().as_ref(), file));
                        count.set_label(&matches.to_string());
                        count.set_visible(true);
                    }
                    Row::Hit { hit, .. } => {
                        line.set_label(&(hit.start.0 + 1).to_string());
                        line.set_visible(true);
                        text.set_markup(&hit.markup);
                        count.set_visible(false);
                    }
                }
            }
        ));
        // A click shows a match at once. Nothing is selected, as rows would
        // otherwise be as the pointer passes over them.
        let view = gtk::ListView::builder()
            .model(&gtk::NoSelection::new(Some(results)))
            .factory(&factory)
            .single_click_activate(true)
            .css_classes(["navigation-sidebar"])
            .build();
        view.connect_activate(glib::clone!(
            #[weak(rename_to = search)]
            self,
            move |_, position| search.show(position)
        ));
        gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(&view)
            .build()
    }

    /// Searches anew, for what the page says now.
    fn rescan(&self) {
        let imp = self.imp();
        self.stop();
        self.results().remove_all();
        imp.counts.set((0, 0));
        let query = Query::new(&imp.settings);
        let error = (!query.text.is_empty())
            .then(|| query.regex().err())
            .flatten();
        show_error(&imp.entry, error.as_ref());
        let root = (imp.root.borrow().as_ref()).and_then(gio::File::path);
        if let (Some(root), Some(window), true) = (
            root,
            self.window(),
            !query.text.is_empty() && error.is_none(),
        ) {
            let open: HashMap<PathBuf, String> = (window.editors())
                .filter_map(|editor| {
                    let buffer = editor.buffer();
                    let text = buffer.text(&buffer.start_iter(), &buffer.end_iter(), false);
                    Some((editor.file().path()?, text.into()))
                })
                .collect();
            let cancellable = gio::Cancellable::new();
            let mut found = scan::scan(root, query, open, cancellable.clone());
            let task = glib::spawn_future_local(glib::clone!(
                #[weak(rename_to = search)]
                self,
                async move {
                    while let Some(found) = found.next().await {
                        search.add(found);
                    }
                    search.imp().scan.take();
                    search.refresh();
                }
            ));
            imp.scan.replace(Some((cancellable, task)));
        }
        self.refresh();
    }

    /// Stops the search under way, if any.
    fn stop(&self) {
        if let Some((cancellable, task)) = self.imp().scan.take() {
            cancellable.cancel();
            task.abort();
        }
    }

    /// Lists a file's matches.
    fn add(&self, found: Found) {
        let imp = self.imp();
        let file = gio::File::for_path(found.path);
        let (matches, files) = imp.counts.get();
        imp.counts.set((matches + found.hits.len(), files + 1));
        let mut rows = vec![Row::File {
            file: file.clone(),
            count: found.hits.len(),
        }];
        rows.extend((found.hits.into_iter()).map(|hit| Row::Hit {
            file: file.clone(),
            hit,
        }));
        let rows: Vec<glib::BoxedAnyObject> =
            rows.into_iter().map(glib::BoxedAnyObject::new).collect();
        self.results().extend_from_slice(&rows);
        self.refresh();
    }

    /// Shows how the search stands, and what can be done.
    fn refresh(&self) {
        let imp = self.imp();
        let searching = imp.scan.borrow().is_some();
        let (matches, files) = imp.counts.get();
        let replacement_error = (imp.settings.is_regex_enabled())
            .then(|| glib::Regex::check_replacement(imp.replacement.text()).err())
            .flatten();
        show_error(&imp.replacement, replacement_error.as_ref());
        imp.replace
            .set_sensitive(!searching && matches > 0 && replacement_error.is_none());
        let label = match (matches, files) {
            (0, _) if searching => "Searching…".to_owned(),
            // Nothing to find, or a pattern that will not do, as it shows.
            (0, _) if imp.entry.text().is_empty() || imp.entry.has_css_class("error") => {
                String::new()
            }
            (0, _) => "No matches".to_owned(),
            // The search stopped there.
            _ if matches > scan::LIMIT => format!("More than {} matches", scan::LIMIT),
            (1, _) => "1 match".to_owned(),
            (_, 1) => format!("{matches} matches"),
            _ => format!("{matches} matches in {files} files"),
        };
        imp.status.set_label(&label);
    }

    fn show_first(&self) {
        self.show(0);
    }

    /// Opens the match at `position` in the results, or the first match of
    /// the file there, selecting it.
    fn show(&self, position: u32) {
        let results = self.results();
        let Some((file, hit)) = [position, position + 1]
            .into_iter()
            .filter_map(|position| {
                results
                    .item(position)
                    .and_downcast::<glib::BoxedAnyObject>()
            })
            .find_map(|object| match &*object.borrow::<Row>() {
                Row::Hit { file, hit } => Some((file.clone(), hit.clone())),
                Row::File { .. } => None,
            })
        else {
            return;
        };
        let Some(window) = self.window() else {
            return;
        };
        glib::spawn_future_local(async move {
            let Some(editor) = Window::open(&window.downgrade(), file).await else {
                return;
            };
            let buffer = editor.buffer();
            let at = |(line, character)| {
                (buffer.iter_at_line_offset(line, character))
                    .or_else(|| buffer.iter_at_line(line))
                    .unwrap_or_else(|| buffer.end_iter())
            };
            editor.reveal(&at(hit.start), &at(hit.end));
        });
    }

    /// Replaces every match in every file listed, opening each, in one edit
    /// to undo per file, leaving the files to save. Then searches anew.
    fn replace_all(&self) {
        let imp = self.imp();
        let Some(window) = self.window() else {
            return;
        };
        let files: Vec<gio::File> = (self.results().iter::<glib::BoxedAnyObject>())
            .flatten()
            .filter_map(|object| match &*object.borrow::<Row>() {
                Row::File { file, .. } => Some(file.clone()),
                Row::Hit { .. } => None,
            })
            .collect();
        let settings = Query::new(&imp.settings).settings();
        let replacement = imp.replacement.text();
        let search = self.downgrade();
        glib::spawn_future_local(async move {
            let window = window.downgrade();
            for file in files {
                if let Some(editor) = Window::open(&window, file).await {
                    let context = sourceview::SearchContext::new(&editor.buffer(), Some(&settings));
                    context.set_highlight(false);
                    // A replacement that will not do shows as such already.
                    context.replace_all(&replacement).ok();
                }
            }
            if let Some(search) = search.upgrade() {
                search.rescan();
            }
        });
    }

    fn results(&self) -> &gio::ListStore {
        self.imp().results.get().expect("the results are set up")
    }

    fn window(&self) -> Option<Window> {
        self.root().and_downcast()
    }
}

/// A heading for `file`'s matches: its name in bold, then the folder it is
/// in under `root`, dimmed.
fn heading(root: Option<&gio::File>, file: &gio::File) -> String {
    let name = file.basename().unwrap_or_default();
    let folder = (file.parent().zip(root))
        .and_then(|(parent, root)| root.relative_path(&parent))
        .unwrap_or_default();
    format!(
        "<b>{}</b>  <span alpha=\"55%\">{}</span>",
        glib::markup_escape_text(&name.to_string_lossy()),
        glib::markup_escape_text(&folder.to_string_lossy())
    )
}

/// A handler, for a signal of any object, that calls `method` on `search`
/// while it lives.
fn calls<T>(search: &ProjectSearch, method: fn(&ProjectSearch)) -> impl Fn(&T) + 'static {
    let search = search.downgrade();
    move |_| {
        if let Some(search) = search.upgrade() {
            method(&search);
        }
    }
}

fn list_item(item: &glib::Object) -> &gtk::ListItem {
    item.downcast_ref().expect("list views use list items")
}
