//! A project window: the file tree beside tabbed editors.

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::{gio, glib};

use crate::editor::Editor;
use crate::{display_name, file_tree};

mod imp {
    use std::cell::RefCell;

    use adw::prelude::*;
    use adw::subclass::prelude::*;
    use gtk::{gdk, gio, glib};

    use super::editor;
    use crate::editor::Editor;
    use crate::lsp;

    #[derive(Default)]
    pub struct Window {
        pub(super) project: RefCell<Option<gio::File>>,
        pub(super) toasts: adw::ToastOverlay,
        pub(super) split_view: adw::OverlaySplitView,
        pub(super) sidebar: gtk::ScrolledWindow,
        pub(super) sidebar_button: gtk::ToggleButton,
        pub(super) heading: adw::WindowTitle,
        pub(super) stack: gtk::Stack,
        pub(super) tabs: adw::TabView,
        pub(super) servers: lsp::Servers,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for Window {
        const NAME: &'static str = "CraftWindow";
        type Type = super::Window;
        type ParentType = adw::ApplicationWindow;

        fn class_init(klass: &mut Self::Class) {
            klass.install_action("win.open-folder", None, |win, _, _| win.open_folder());
            klass.install_action("win.save", None, |win, _, _| win.save());
            klass.install_action("win.close-tab", None, |win, _, _| win.close_tab());
            klass.install_action("win.toggle-sidebar", None, |win, _, _| win.toggle_sidebar());
            klass.install_action("win.go-to-definition", None, |win, _, _| {
                win.go_to_definition();
            });
            let ctrl = gdk::ModifierType::CONTROL_MASK;
            let none = gdk::ModifierType::empty();
            klass.add_binding_action(gdk::Key::o, ctrl, "win.open-folder");
            klass.add_binding_action(gdk::Key::s, ctrl, "win.save");
            klass.add_binding_action(gdk::Key::w, ctrl, "win.close-tab");
            klass.add_binding_action(gdk::Key::F9, none, "win.toggle-sidebar");
            klass.add_binding_action(gdk::Key::F12, none, "win.go-to-definition");
        }
    }

    impl ObjectImpl for Window {
        fn constructed(&self) {
            self.parent_constructed();
            let obj = self.obj();
            obj.set_title(Some("Craft"));
            obj.set_default_size(1000, 700);

            self.sidebar.set_hscrollbar_policy(gtk::PolicyType::Never);
            let sidebar = adw::ToolbarView::new();
            sidebar.add_top_bar(&adw::HeaderBar::new());
            sidebar.set_content(Some(&self.sidebar));

            self.sidebar_button.set_icon_name("sidebar-show-symbolic");
            self.sidebar_button.set_tooltip_text(Some("Toggle Sidebar"));
            self.sidebar_button.set_visible(false);
            self.sidebar_button
                .bind_property("active", &self.split_view, "show-sidebar")
                .bidirectional()
                .sync_create()
                .build();
            let header = adw::HeaderBar::builder()
                .title_widget(&self.heading)
                .build();
            header.pack_start(&self.sidebar_button);
            header.pack_end(self.servers.indicator());

            let open = gtk::Button::builder()
                .label("_Open Folder…")
                .use_underline(true)
                .action_name("win.open-folder")
                .halign(gtk::Align::Center)
                .css_classes(["pill", "suggested-action"])
                .build();
            let welcome = adw::StatusPage::builder()
                .title("Open a Folder")
                .child(&open)
                .build();
            self.stack.add_named(&welcome, Some("welcome"));
            self.stack.add_named(&self.tabs, Some("tabs"));

            let content = adw::ToolbarView::new();
            content.add_top_bar(&header);
            content.add_top_bar(&adw::TabBar::builder().view(&self.tabs).build());
            content.set_content(Some(&self.stack));

            self.split_view.set_sidebar(Some(&sidebar));
            self.split_view.set_content(Some(&content));
            self.toasts.set_child(Some(&self.split_view));
            obj.set_content(Some(&self.toasts));

            // These would take Ctrl+Home/End, with or without Shift, from text.
            self.tabs.remove_shortcuts(
                adw::TabViewShortcuts::CONTROL_HOME
                    | adw::TabViewShortcuts::CONTROL_END
                    | adw::TabViewShortcuts::CONTROL_SHIFT_HOME
                    | adw::TabViewShortcuts::CONTROL_SHIFT_END,
            );

            self.tabs.connect_close_page(glib::clone!(
                #[weak]
                obj,
                #[upgrade_or]
                glib::Propagation::Proceed,
                move |tabs, page| {
                    let editor = editor(page);
                    if !editor.is_modified() {
                        return glib::Propagation::Proceed;
                    }
                    let (tabs, page) = (tabs.clone(), page.clone());
                    glib::spawn_future_local(async move {
                        tabs.close_page_finish(&page, obj.confirm_close(&[editor]).await);
                    });
                    glib::Propagation::Stop
                }
            ));
            self.tabs.connect_selected_page_notify(glib::clone!(
                #[weak]
                obj,
                move |_| obj.update_title()
            ));
            self.tabs.connect_n_pages_notify(glib::clone!(
                #[weak]
                obj,
                move |_| obj.update_state()
            ));
            obj.update_state();
        }

        fn dispose(&self) {
            self.servers.shutdown();
        }
    }

    impl WidgetImpl for Window {}

    impl WindowImpl for Window {
        fn close_request(&self) -> glib::Propagation {
            let obj = self.obj();
            let modified: Vec<Editor> = obj.editors().filter(Editor::is_modified).collect();
            if modified.is_empty() {
                return self.parent_close_request();
            }
            if obj.visible_dialog().is_none() {
                let obj = obj.clone();
                glib::spawn_future_local(async move {
                    if obj.confirm_close(&modified).await {
                        obj.destroy();
                    }
                });
            }
            glib::Propagation::Stop
        }
    }

    impl ApplicationWindowImpl for Window {}
    impl AdwApplicationWindowImpl for Window {}
}

glib::wrapper! {
    pub struct Window(ObjectSubclass<imp::Window>)
        @extends adw::ApplicationWindow, gtk::ApplicationWindow, gtk::Window, gtk::Widget,
        @implements gio::ActionGroup, gio::ActionMap, gtk::Accessible, gtk::Buildable,
            gtk::ConstraintTarget, gtk::Native, gtk::Root, gtk::ShortcutManager;
}

impl Window {
    pub fn new(app: &adw::Application) -> Self {
        glib::Object::builder().property("application", app).build()
    }

    /// The directory shown in the sidebar.
    pub fn project(&self) -> Option<gio::File> {
        self.imp().project.borrow().clone()
    }

    pub fn set_project(&self, root: &gio::File) {
        let imp = self.imp();
        imp.project.replace(Some(root.clone()));
        imp.servers.set_root(root, self.editors());
        let tree = file_tree::new(
            root,
            glib::clone!(
                #[weak(rename_to = win)]
                self,
                move |file| win.open_files(vec![file])
            ),
        );
        imp.sidebar.set_child(Some(&tree));
        imp.sidebar_button.set_visible(true);
        imp.split_view.set_show_sidebar(true);
        self.set_title(Some(&display_name(root)));
        self.update_state();
        self.update_title();
    }

    /// Shows `files`, reading each that is not open yet into a new tab. Tabs
    /// are added in order, however long each file takes to read.
    pub fn open_files(&self, files: Vec<gio::File>) {
        let win = self.downgrade();
        glib::spawn_future_local(async move {
            for file in files {
                Self::open(&win, file).await;
            }
        });
    }

    /// Selects the tab editing `file`, reading the file into a new tab first
    /// if needed.
    async fn open(win: &glib::WeakRef<Self>, file: gio::File) -> Option<Editor> {
        if let Some(editor) = win.upgrade()?.select(&file) {
            return Some(editor);
        }
        let result = Editor::open(file.clone()).await;
        let win = win.upgrade()?;
        match result {
            // The same file may have been opened while this one was read.
            Ok(editor) => Some(win.select(&file).unwrap_or_else(|| {
                win.add(&editor);
                editor
            })),
            Err(err) => {
                win.report("open", &file, &err);
                None
            }
        }
    }

    fn add(&self, editor: &Editor) {
        let tabs = &self.imp().tabs;
        let page = tabs.append(editor);
        // Connected before the binding below sets the first title, as a first
        // tab is already selected.
        page.connect_title_notify(glib::clone!(
            #[weak(rename_to = win)]
            self,
            move |page| {
                if page.is_selected() {
                    win.imp().heading.set_title(&page.title());
                }
            }
        ));
        editor
            .bind_property("title", &page, "title")
            .sync_create()
            .build();
        page.set_tooltip(&glib::markup_escape_text(&editor.file().parse_name()));
        tabs.set_selected_page(&page);
        editor.grab_focus();
        self.imp().servers.attach(editor);
    }

    /// Selects and focuses the tab editing `file`, if there is one.
    fn select(&self, file: &gio::File) -> Option<Editor> {
        let tabs = &self.imp().tabs;
        let editor = self.editors().find(|editor| editor.file().equal(file))?;
        tabs.set_selected_page(&tabs.page(&editor));
        editor.grab_focus();
        Some(editor)
    }

    fn current_editor(&self) -> Option<Editor> {
        self.imp().tabs.selected_page().map(|page| editor(&page))
    }

    fn editors(&self) -> impl Iterator<Item = Editor> + '_ {
        let tabs = &self.imp().tabs;
        (0..tabs.n_pages()).map(|i| editor(&tabs.nth_page(i)))
    }

    fn open_folder(&self) {
        let win = self.downgrade();
        gtk::FileDialog::builder()
            .title("Open Folder")
            .build()
            .select_folder(Some(self), gio::Cancellable::NONE, move |result| {
                let Some(win) = win.upgrade() else { return };
                match result {
                    Ok(folder) => win
                        .application()
                        .expect("windows belong to the app")
                        .open(&[folder], ""),
                    Err(err) if !err.matches(gtk::DialogError::Dismissed) => {
                        win.toast(&format!("Could not open folder: {}", err.message()));
                    }
                    Err(_) => {}
                }
            });
    }

    fn save(&self) {
        let Some(editor) = self.current_editor() else {
            return;
        };
        let win = self.downgrade();
        glib::spawn_future_local(async move {
            if let Err(err) = editor.save().await
                && let Some(win) = win.upgrade()
            {
                win.report("save", &editor.file(), &err);
            }
        });
    }

    /// Shows where what is at the cursor is defined, as the language server
    /// says.
    fn go_to_definition(&self) {
        let Some(editor) = self.current_editor() else {
            return;
        };
        let buffer = editor.buffer();
        let cursor = buffer.iter_at_mark(&buffer.get_insert());
        let request = editor
            .document()
            .map(|document| (document.definition(&cursor), document.encoding()));
        let win = self.downgrade();
        glib::spawn_future_local(async move {
            let found = match request {
                Some((definition, encoding)) => definition.await.map(|found| (found, encoding)),
                None => None,
            };
            match found {
                Some(((file, position), encoding)) => {
                    if let Some(editor) = Self::open(&win, file).await {
                        editor.goto(position, encoding);
                    }
                }
                None => {
                    if let Some(win) = win.upgrade() {
                        win.toast("No definition found");
                    }
                }
            }
        });
    }

    fn close_tab(&self) {
        let tabs = &self.imp().tabs;
        if let Some(page) = tabs.selected_page() {
            tabs.close_page(&page);
        }
    }

    fn toggle_sidebar(&self) {
        let split_view = &self.imp().split_view;
        split_view.set_show_sidebar(!split_view.shows_sidebar());
    }

    /// Asks whether to save `editors` before closing them, saving them if so.
    /// Returns whether to go ahead and close.
    async fn confirm_close(&self, editors: &[Editor]) -> bool {
        let body = match editors {
            [editor] => format!("“{}” has unsaved changes.", display_name(&editor.file())),
            _ => format!("{} files have unsaved changes.", editors.len()),
        };
        let dialog = adw::AlertDialog::new(Some("Save Changes?"), Some(&body));
        dialog.add_responses(&[
            ("cancel", "_Cancel"),
            ("discard", "_Discard"),
            ("save", "_Save"),
        ]);
        dialog.set_response_appearance("discard", adw::ResponseAppearance::Destructive);
        dialog.set_response_appearance("save", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("save"));
        dialog.set_close_response("cancel");
        match dialog.choose_future(Some(self)).await.as_str() {
            "discard" => true,
            "save" => {
                for editor in editors {
                    if let Err(err) = editor.save().await {
                        self.report("save", &editor.file(), &err);
                        return false;
                    }
                }
                editors.iter().all(|editor| !editor.is_modified())
            }
            _ => false,
        }
    }

    /// Enables what applies and shows the welcome page while there is nothing
    /// else to show.
    fn update_state(&self) {
        let imp = self.imp();
        let has_tabs = imp.tabs.n_pages() > 0;
        let has_project = imp.project.borrow().is_some();
        self.action_set_enabled("win.save", has_tabs);
        self.action_set_enabled("win.close-tab", has_tabs);
        self.action_set_enabled("win.go-to-definition", has_tabs);
        self.action_set_enabled("win.toggle-sidebar", has_project);
        imp.stack
            .set_visible_child_name(if has_tabs || has_project {
                "tabs"
            } else {
                "welcome"
            });
    }

    /// Shows the selected tab's title and location in the header bar.
    fn update_title(&self) {
        let imp = self.imp();
        let page = imp.tabs.selected_page();
        imp.heading
            .set_title(&page.as_ref().map(adw::TabPage::title).unwrap_or_default());
        imp.heading.set_subtitle(
            &page
                .map(|page| self.location(&editor(&page).file()))
                .unwrap_or_default(),
        );
    }

    /// The directory containing `file`: relative to the project when inside
    /// it, empty at its root.
    fn location(&self, file: &gio::File) -> String {
        let Some(dir) = file.parent() else {
            return String::new();
        };
        let project = self.project();
        if project.as_ref().is_some_and(|root| root.equal(&dir)) {
            return String::new();
        }
        project
            .and_then(|root| root.relative_path(&dir))
            .map_or_else(
                || dir.parse_name().into(),
                |path| glib::filename_display_name(path).into(),
            )
    }

    /// Shows `err` unless it is a cancellation, which the user asked for.
    fn report(&self, action: &str, file: &gio::File, err: &glib::Error) {
        if !err.matches(gio::IOErrorEnum::Cancelled) {
            self.toast(&format!(
                "Could not {action} “{}”: {}",
                display_name(file),
                err.message()
            ));
        }
    }

    fn toast(&self, message: &str) {
        self.imp().toasts.add_toast(
            adw::Toast::builder()
                .title(message)
                .use_markup(false)
                .build(),
        );
    }
}

/// The editor shown by `page`.
fn editor(page: &adw::TabPage) -> Editor {
    page.child().downcast().expect("every page holds an editor")
}
