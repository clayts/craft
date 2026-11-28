//! Finding and replacing in the selected tab: a bar under the tabs that
//! highlights each match of what is typed into it, says how many there are
//! and which is selected, moves between them, and replaces them.

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::glib;
use sourceview::prelude::*;

use crate::editor::Editor;

/// The style class of an editor being searched, which `style.css` shows the
/// selection of as when the editor has the focus, so that the selected match
/// stands out from the rest while the bar has the focus.
const SEARCHED: &str = "searched";

mod imp {
    use std::cell::RefCell;

    use adw::subclass::prelude::*;
    use gtk::glib;

    use super::Target;
    use crate::editor::Editor;

    #[derive(Default)]
    pub struct Search {
        pub(super) bar: gtk::SearchBar,
        pub(super) entry: gtk::SearchEntry,
        /// How many matches there are, and which is selected.
        pub(super) count: gtk::Label,
        /// The buttons that move between matches.
        pub(super) moves: gtk::Box,
        /// Whether the bar shows the replacement too.
        pub(super) replacing: gtk::ToggleButton,
        pub(super) replacement: gtk::Entry,
        /// The buttons that replace matches.
        pub(super) replaces: gtk::Box,
        /// What to find and how, in whichever editor.
        pub(super) settings: sourceview::SearchSettings,
        /// The selected tab's editor.
        pub(super) editor: RefCell<Option<Editor>>,
        /// The search in it, while the bar is open.
        pub(super) target: RefCell<Option<Target>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for Search {
        const NAME: &'static str = "CraftSearch";
        type Type = super::Search;
        type ParentType = adw::Bin;
    }

    impl ObjectImpl for Search {
        fn constructed(&self) {
            self.parent_constructed();
            self.obj().set_up();
        }

        fn dispose(&self) {
            self.target.take();
        }
    }

    impl WidgetImpl for Search {}
    impl BinImpl for Search {}
}

glib::wrapper! {
    pub struct Search(ObjectSubclass<imp::Search>)
        @extends adw::Bin, gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl Default for Search {
    fn default() -> Self {
        glib::Object::new()
    }
}

/// The search in an editor, while the bar is open on it.
struct Target {
    editor: Editor,
    context: sourceview::SearchContext,
    /// Keeps the count up to date as the selection moves.
    moved: Option<glib::SignalHandlerId>,
}

impl Target {
    fn new(search: &Search, editor: Editor) -> Self {
        editor.view().add_css_class(SEARCHED);
        let buffer = editor.buffer();
        let context = sourceview::SearchContext::new(&buffer, Some(&search.imp().settings));
        // Once the whole text is searched, which is also when any error in
        // the pattern is final.
        context.connect_occurrences_count_notify(calls(search, Search::refresh));
        let moved = buffer.connect_cursor_position_notify(calls(search, Search::refresh));
        Self {
            editor,
            context,
            moved: Some(moved),
        }
    }
}

impl Drop for Target {
    fn drop(&mut self) {
        if let Some(moved) = self.moved.take() {
            self.editor.buffer().disconnect(moved);
        }
        self.editor.view().remove_css_class(SEARCHED);
    }
}

impl Search {
    /// Searches in `editor`, the selected tab's, or closes with none.
    pub fn set_editor(&self, editor: Option<Editor>) {
        let imp = self.imp();
        let closing = editor.is_none();
        imp.editor.replace(editor);
        if closing {
            self.close();
        }
        self.retarget();
    }

    /// Opens the bar to find, and if `replacing`, to replace, what is
    /// selected, if it is on one line and not found already, or else what
    /// the bar had.
    pub fn open(&self, replacing: bool) {
        let imp = self.imp();
        if imp.editor.borrow().is_none() {
            return;
        }
        imp.replacing.set_active(replacing);
        imp.bar.set_search_mode(true);
        if let Some((editor, context)) = self.target() {
            let buffer = editor.buffer();
            if let Some((start, end)) = buffer.selection_bounds()
                && start.line() == end.line()
                && !is_match(&context, &start, &end)
            {
                let text = buffer.text(&start, &end, false);
                imp.entry.set_text(&if imp.settings.is_regex_enabled() {
                    glib::Regex::escape_string(&text)
                } else {
                    text
                });
            }
        }
        let field: &gtk::Editable = if replacing && !imp.entry.text().is_empty() {
            imp.replacement.upcast_ref()
        } else {
            imp.entry.upcast_ref()
        };
        field.grab_focus();
        field.select_region(0, -1);
        self.search_changed();
    }

    /// Selects the next match after the selection, wrapping around, opening
    /// the bar if need be, or asking what to find if nothing is given yet.
    pub fn find_next(&self) {
        self.find(true);
    }

    /// As [`Self::find_next`], but the previous match before the selection.
    pub fn find_previous(&self) {
        self.find(false);
    }

    /// Lays the bar out and connects it up.
    fn set_up(&self) {
        let imp = self.imp();
        imp.settings.set_wrap_around(true);

        let entry = &imp.entry;
        entry.set_placeholder_text(Some("Find"));
        entry.set_hexpand(true);
        entry.connect_search_changed(calls(self, Self::search_changed));
        entry.connect_activate(calls(self, Self::find_next));
        entry.connect_next_match(calls(self, Self::find_next));
        entry.connect_previous_match(calls(self, Self::find_previous));
        entry.connect_stop_search(calls(self, Self::close));
        entry.add_controller(shortcut(self, "<Shift>Return", Self::find_previous));
        // Escape leaves the bar from anywhere in it. In the editor, it is the
        // editor's, to dismiss completions and the like.
        self.add_controller(shortcut(self, "Escape", Self::close));

        imp.count.set_width_chars(10);
        imp.count.add_css_class("dim-label");
        imp.count.add_css_class("numeric");
        imp.moves.add_css_class("linked");
        for (icon, tooltip, method) in [
            (
                "go-up-symbolic",
                "Previous Match",
                Self::find_previous as fn(&Self),
            ),
            ("go-down-symbolic", "Next Match", Self::find_next),
        ] {
            let button = gtk::Button::builder()
                .icon_name(icon)
                .tooltip_text(tooltip)
                .build();
            button.connect_clicked(calls(self, method));
            imp.moves.append(&button);
        }
        let controls = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        controls.append(&imp.count);
        controls.append(&imp.moves);
        for option in options(&imp.settings) {
            controls.append(&option);
        }
        imp.replacing.set_icon_name("edit-find-replace-symbolic");
        imp.replacing.set_tooltip_text(Some("Replace"));
        controls.append(&imp.replacing);

        imp.replacement.set_placeholder_text(Some("Replace"));
        imp.replacement.connect_activate(calls(self, Self::replace));
        imp.replacement.connect_changed(calls(self, Self::refresh));
        imp.replaces.set_spacing(6);
        imp.replaces.set_homogeneous(true);
        for (label, method) in [
            ("_Replace", Self::replace as fn(&Self)),
            ("Replace _All", Self::replace_all),
        ] {
            let button = gtk::Button::with_mnemonic(label);
            button.connect_clicked(calls(self, method));
            imp.replaces.append(&button);
        }
        for row in [
            imp.replacement.upcast_ref::<gtk::Widget>(),
            imp.replaces.upcast_ref(),
        ] {
            imp.replacing
                .bind_property("active", row, "visible")
                .sync_create()
                .build();
        }

        let grid = gtk::Grid::builder()
            .column_spacing(6)
            .row_spacing(6)
            .build();
        grid.attach(entry, 0, 0, 1, 1);
        grid.attach(&controls, 1, 0, 1, 1);
        grid.attach(&imp.replacement, 0, 1, 1, 1);
        grid.attach(&imp.replaces, 1, 1, 1, 1);
        let clamp = adw::Clamp::builder()
            .maximum_size(640)
            .hexpand(true)
            .child(&grid)
            .build();
        imp.bar.set_child(Some(&clamp));
        imp.bar.set_show_close_button(true);
        imp.bar
            .connect_search_mode_enabled_notify(calls(self, Self::opened_or_closed));
        self.set_child(Some(&imp.bar));
    }

    fn close(&self) {
        self.imp().bar.set_search_mode(false);
    }

    /// Searches in the editor while the bar is open. On closing, the focus
    /// leaves the bar for the editor, so that typing goes on there.
    fn opened_or_closed(&self) {
        let imp = self.imp();
        let editor = imp.editor.borrow().clone();
        if !imp.bar.is_search_mode()
            && imp.bar.focus_child().is_some()
            && let Some(editor) = editor
        {
            editor.grab_focus();
        }
        self.retarget();
    }

    /// Selects the next match after the selection, or if not `forward`, the
    /// previous one before it, as [`Self::find_next`] says.
    fn find(&self, forward: bool) {
        let imp = self.imp();
        if imp.entry.text().is_empty() {
            self.open(false);
            return;
        }
        imp.bar.set_search_mode(true);
        self.search_changed();
        let Some((editor, context)) = self.target() else {
            return;
        };
        let (start, end) = selection(&editor.buffer());
        let found = if forward {
            context.forward(&end)
        } else {
            context.backward(&start)
        };
        if let Some((start, end, _)) = found {
            select(&editor, &start, &end);
        }
    }

    /// Searches for what the entry has, if that is new, from the start of
    /// the selection, so that each letter typed narrows the match selected.
    fn search_changed(&self) {
        let imp = self.imp();
        let text = imp.entry.text();
        if imp.settings.search_text().unwrap_or_default() == text {
            return;
        }
        imp.settings.set_search_text(Some(&text));
        if let Some((editor, context)) = self.target() {
            let (start, _) = selection(&editor.buffer());
            if let Some((start, end, _)) = context.forward(&start) {
                select(&editor, &start, &end);
            }
        }
        self.refresh();
    }

    /// Replaces the selected match, if the selection is one, then selects
    /// the next.
    fn replace(&self) {
        self.search_changed();
        let Some((editor, context)) = self.target() else {
            return;
        };
        let buffer = editor.buffer();
        let (mut start, mut end) = selection(&buffer);
        if is_match(&context, &start, &end) {
            let replacement = self.imp().replacement.text();
            // A replacement that will not do shows as such already.
            if (context.replace(&mut start, &mut end, &replacement)).is_err() {
                return;
            }
            buffer.place_cursor(&end);
        }
        self.find_next();
    }

    /// Replaces every match, in one edit to undo.
    fn replace_all(&self) {
        self.search_changed();
        if let Some((_, context)) = self.target() {
            // A replacement that will not do shows as such already.
            context.replace_all(&self.imp().replacement.text()).ok();
        }
    }

    /// Shows how the search stands: whether what it is given will do, how
    /// many matches there are and which is selected, and what can be done.
    fn refresh(&self) {
        let imp = self.imp();
        let Some((editor, context)) = self.target() else {
            return;
        };
        let pattern_error = context.regex_error();
        let replacement_error = (imp.settings.is_regex_enabled())
            .then(|| glib::Regex::check_replacement(imp.replacement.text()).err())
            .flatten();
        show_error(&imp.entry, pattern_error.as_ref());
        show_error(&imp.replacement, replacement_error.as_ref());
        let searching = imp.settings.search_text().is_some() && pattern_error.is_none();
        let count = context.occurrences_count();
        imp.moves.set_sensitive(searching && count != 0);
        imp.replaces
            .set_sensitive(searching && count != 0 && replacement_error.is_none());
        let label = match count {
            _ if !searching => String::new(),
            // Unknown until the text is searched through, when this runs
            // again; until then, the count shown stands.
            ..0 => return,
            0 => "No matches".to_owned(),
            _ => {
                let (start, end) = selection(&editor.buffer());
                match context.occurrence_position(&start, &end) {
                    position if position > 0 => format!("{position} of {count}"),
                    _ if count == 1 => "1 match".to_owned(),
                    _ => format!("{count} matches"),
                }
            }
        };
        imp.count.set_label(&label);
    }

    /// Searches in the selected tab's editor while the bar is open, and in
    /// none while it is closed.
    fn retarget(&self) {
        let imp = self.imp();
        let editor = (imp.editor.borrow().clone()).filter(|_| imp.bar.is_search_mode());
        let current = (imp.target.borrow().as_ref()).map(|target| target.editor.clone());
        if editor == current {
            return;
        }
        let target = editor.map(|editor| Target::new(self, editor));
        // The old one goes, taking its highlighting with it, and its count.
        imp.target.replace(target);
        imp.count.set_label("");
        self.refresh();
    }

    /// The editor searched in, and its search, while the bar is open.
    fn target(&self) -> Option<(Editor, sourceview::SearchContext)> {
        let target = self.imp().target.borrow();
        (target.as_ref()).map(|target| (target.editor.clone(), target.context.clone()))
    }
}

/// A handler, for a signal of any object, that calls `method` on `search`
/// while it lives.
fn calls<T>(search: &Search, method: fn(&Search)) -> impl Fn(&T) + 'static {
    let search = search.downgrade();
    move |_| {
        if let Some(search) = search.upgrade() {
            method(&search);
        }
    }
}

/// A controller that calls `method` on `search` on the keys `trigger` names.
fn shortcut(search: &Search, trigger: &str, method: fn(&Search)) -> gtk::ShortcutController {
    let call = calls(search, method);
    let action = gtk::CallbackAction::new(move |widget, _| {
        call(widget);
        glib::Propagation::Stop
    });
    let controller = gtk::ShortcutController::new();
    controller.add_shortcut(gtk::Shortcut::new(
        gtk::ShortcutTrigger::parse_string(trigger),
        Some(action),
    ));
    controller
}

/// Toggles for how `settings` finds: matching case, whole words, and
/// regular expressions.
pub fn options(settings: &sourceview::SearchSettings) -> [gtk::ToggleButton; 3] {
    [
        ("Aa", "Match Case", "case-sensitive"),
        ("<u>ab</u>", "Match Whole Words", "at-word-boundaries"),
        (".*", "Regular Expressions", "regex-enabled"),
    ]
    .map(|(label, tooltip, setting)| {
        let option = gtk::ToggleButton::builder()
            .child(&gtk::Label::builder().label(label).use_markup(true).build())
            .tooltip_text(tooltip)
            .css_classes(["flat"])
            .build();
        settings
            .bind_property(setting, &option, "active")
            .bidirectional()
            .sync_create()
            .build();
        option
    })
}

/// The bounds of the selection, or the cursor's position twice.
fn selection(buffer: &sourceview::Buffer) -> (gtk::TextIter, gtk::TextIter) {
    buffer.selection_bounds().unwrap_or_else(|| {
        let cursor = buffer.iter_at_mark(&buffer.get_insert());
        (cursor, cursor)
    })
}

/// Whether `context` finds a match from `start` to `end`.
fn is_match(
    context: &sourceview::SearchContext,
    start: &gtk::TextIter,
    end: &gtk::TextIter,
) -> bool {
    context
        .forward(start)
        .is_some_and(|(found, found_end, _)| (&found, &found_end) == (start, end))
}

/// Selects from `start` to `end` in `editor`, scrolling it into view.
fn select(editor: &Editor, start: &gtk::TextIter, end: &gtk::TextIter) {
    let buffer = editor.buffer();
    buffer.select_range(start, end);
    editor
        .view()
        .scroll_to_mark(&buffer.get_insert(), 0.25, false, 0.0, 0.0);
}

/// Marks `widget` as holding what will not do, saying why, or as fine.
pub fn show_error(widget: &impl IsA<gtk::Widget>, error: Option<&glib::Error>) {
    if error.is_some() {
        widget.add_css_class("error");
    } else {
        widget.remove_css_class("error");
    }
    widget.set_tooltip_text(error.map(glib::Error::message));
}
