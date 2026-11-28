//! The hover: once the pointer rests on text, a popover over it shows the
//! diagnostics there and the language server's description of what it is.
//! It stays while the pointer is over what it describes or over itself, to
//! be scrolled through and have its text selected and copied, and goes once
//! the pointer has left both, or the text is typed in, clicked or scrolled.

use std::time::Duration;

use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{gdk, glib, graphene, pango};

use crate::editor::Editor;

/// How long the pointer rests on text before the hover shows.
const DELAY: Duration = Duration::from_millis(500);

/// How long the hover stays once the pointer has left it and what it
/// describes, for the pointer to cross between the two.
const GRACE: Duration = Duration::from_millis(300);

/// How long to wait for the server's description before showing the rest.
const TIMEOUT: Duration = Duration::from_secs(2);

/// The space around the hover's text.
const MARGIN: i32 = 8;

mod imp {
    use std::cell::{Cell, RefCell};

    use gtk::prelude::*;
    use gtk::subclass::prelude::*;
    use gtk::{gdk, glib};

    use crate::editor::Editor;

    #[derive(Default)]
    pub struct Hover {
        pub(super) editor: glib::WeakRef<Editor>,
        /// Where the pointer last moved to over the view.
        pub(super) pointer: Cell<(f64, f64)>,
        /// Where what the hover shows is in the view.
        pub(super) area: Cell<Option<gdk::Rectangle>>,
        /// The wait for the pointer to rest.
        pub(super) resting: RefCell<Option<glib::SourceId>>,
        /// The grace before the hover goes, once the pointer has left it.
        pub(super) leaving: RefCell<Option<glib::SourceId>>,
        /// The search for what to show of what the pointer rests on.
        pub(super) lookup: RefCell<Option<glib::JoinHandle<()>>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for Hover {
        const NAME: &'static str = "CraftHover";
        type Type = super::Hover;
        type ParentType = gtk::Popover;
    }

    impl ObjectImpl for Hover {
        fn constructed(&self) {
            self.parent_constructed();
            self.obj().set_up();
        }
    }

    impl WidgetImpl for Hover {
        // The editor takes the focus back from the hover's text, which has
        // it once clicked in, as the hover goes.
        fn unmap(&self) {
            let obj = self.obj();
            let focused = (obj.root().and_then(|root| root.focus()))
                .is_some_and(|focus| focus.is_ancestor(&*obj));
            self.parent_unmap();
            if focused && let Some(editor) = obj.parent() {
                editor.grab_focus();
            }
        }
    }

    impl PopoverImpl for Hover {}
}

glib::wrapper! {
    pub struct Hover(ObjectSubclass<imp::Hover>)
        @extends gtk::Popover, gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget,
            gtk::Native, gtk::ShortcutManager;
}

impl Hover {
    /// The hover over `editor`'s view, which shows what the editor's
    /// document says of what the pointer rests on. It is the editor's, which
    /// places it whenever laid out, so that it follows the text as the view
    /// moves or is resized.
    pub fn new(editor: &Editor) -> Self {
        let hover: Self = glib::Object::new();
        hover.imp().editor.set(Some(editor));
        hover.set_parent(editor);
        hover.watch(&editor.view());
        hover
    }

    fn set_up(&self) {
        self.set_autohide(false);
        self.set_has_arrow(false);
        self.set_position(gtk::PositionType::Top);
        // Its text lines up with what it describes.
        self.set_halign(gtk::Align::Start);
        self.set_offset(-MARGIN, 0);
        self.add_css_class("hover");
        // The pointer stays in the hover as it moves on to the menu of the
        // hover's text, which is the text's.
        let motion = gtk::EventControllerMotion::builder()
            .propagation_limit(gtk::PropagationLimit::None)
            .build();
        motion.connect_enter(glib::clone!(
            #[weak(rename_to = hover)]
            self,
            move |_, _, _| hover.cancel_leaving()
        ));
        motion.connect_leave(glib::clone!(
            #[weak(rename_to = hover)]
            self,
            move |_| hover.leave()
        ));
        self.add_controller(motion);
        let keys = gtk::EventControllerKey::new();
        keys.connect_key_pressed(glib::clone!(
            #[weak(rename_to = hover)]
            self,
            #[upgrade_or]
            glib::Propagation::Proceed,
            move |keys, key, _, _| hover.key_pressed(keys, key)
        ));
        self.add_controller(keys);
    }

    /// Watches `view` for the pointer resting on its text, and for what makes
    /// the hover go.
    fn watch(&self, view: &sourceview::View) {
        let motion = gtk::EventControllerMotion::new();
        motion.connect_motion(glib::clone!(
            #[weak(rename_to = hover)]
            self,
            move |_, x, y| hover.moved(x, y)
        ));
        motion.connect_leave(glib::clone!(
            #[weak(rename_to = hover)]
            self,
            move |_| hover.stop_looking()
        ));
        view.add_controller(motion);
        // Typing, which a modifier pressed alone is not.
        let keys = gtk::EventControllerKey::new();
        keys.connect_key_pressed(glib::clone!(
            #[weak(rename_to = hover)]
            self,
            #[upgrade_or]
            glib::Propagation::Proceed,
            move |keys, _, _, _| {
                if !is_modifier(keys) {
                    hover.dismiss();
                }
                glib::Propagation::Proceed
            }
        ));
        view.add_controller(keys);
        // Clicks, but for right-clicks, which open a menu that acts on what
        // the hover describes.
        let click = gtk::GestureClick::builder()
            .button(0)
            .propagation_phase(gtk::PropagationPhase::Capture)
            .build();
        click.connect_pressed(glib::clone!(
            #[weak(rename_to = hover)]
            self,
            move |click, _, _, _| {
                if click.current_button() != gdk::BUTTON_SECONDARY {
                    hover.dismiss();
                }
            }
        ));
        view.add_controller(click);
        // Edits and scrolls, which move the text from under it.
        view.buffer().connect_changed(self.dismisser());
        for adjustment in [view.hadjustment(), view.vadjustment()]
            .into_iter()
            .flatten()
        {
            adjustment.connect_value_changed(self.dismisser());
        }
        // Switching tabs, after which it would show again.
        view.connect_unmap(self.dismisser());
    }

    /// A signal handler hiding the hover.
    fn dismisser<T>(&self) -> impl Fn(&T) + 'static {
        let hover = self.downgrade();
        move |_: &T| {
            if let Some(hover) = hover.upgrade() {
                hover.dismiss();
            }
        }
    }

    /// Follows the pointer to `x`, `y` in the view: the hover stays while it
    /// is over what the hover describes, and shows again once it rests.
    fn moved(&self, x: f64, y: f64) {
        let imp = self.imp();
        // GTK repeats where the pointer is as what is under it changes.
        if imp.pointer.replace((x, y)) == (x, y) {
            return;
        }
        if self.shows(x, y) {
            self.cancel_leaving();
        } else {
            self.leave();
        }
        let hover = self.downgrade();
        let resting = glib::timeout_add_local_once(DELAY, move || {
            if let Some(hover) = hover.upgrade() {
                hover.imp().resting.take();
                hover.rested(x, y);
            }
        });
        if let Some(resting) = imp.resting.replace(Some(resting)) {
            resting.remove();
        }
    }

    /// Looks up what to show of what the pointer rests on, at `x`, `y` in
    /// the view, unless the hover shows it already.
    fn rested(&self, x: f64, y: f64) {
        if self.shows(x, y) {
            return;
        }
        let Some(editor) = self.imp().editor.upgrade() else {
            return;
        };
        let Some(document) = editor.document() else {
            return;
        };
        let view = editor.view();
        #[expect(clippy::cast_possible_truncation, reason = "pixels fit")]
        let (x, y) = view.window_to_buffer_coords(gtk::TextWindowType::Widget, x as i32, y as i32);
        let Some(iter) = view.iter_at_location(x, y) else {
            return;
        };
        // Spaces have nothing to describe, but may have diagnostics, of what
        // is missing there.
        let messages = document.diagnostics_at(&iter);
        let space = iter.char().is_whitespace();
        if space && messages.is_empty() {
            return;
        }
        let description = (!space).then(|| document.hover(&iter));
        let encoding = document.encoding();
        let (start, end) = word(&iter);
        let (at, start, end) = (iter.offset(), start.offset(), end.offset());
        let lookup = glib::spawn_future_local(glib::clone!(
            #[weak(rename_to = hover)]
            self,
            #[weak]
            view,
            async move {
                let description = match description {
                    Some(description) => (glib::future_with_timeout(TIMEOUT, description).await)
                        .ok()
                        .flatten(),
                    None => None,
                };
                // What the server says it describes, if the pointer is on
                // it, or else the word.
                let buffer = view.buffer();
                let range = description
                    .as_ref()
                    .and_then(|description| description.range);
                let (start, end) = range
                    .map(|range| {
                        (
                            encoding.iter_at(&buffer, range.start),
                            encoding.iter_at(&buffer, range.end),
                        )
                    })
                    .filter(|(start, end)| (start.offset()..end.offset()).contains(&at))
                    .unwrap_or_else(|| (buffer.iter_at_offset(start), buffer.iter_at_offset(end)));
                let parts = description.map(|description| description.parts);
                hover.show(&view, &messages, &parts.unwrap_or_default(), &start, &end);
            }
        ));
        if let Some(lookup) = self.imp().lookup.replace(Some(lookup)) {
            lookup.abort();
        }
    }

    /// Shows `messages` and `parts`, if there are any, over the text from
    /// `start` to `end` in `view`, if the pointer is still on it.
    fn show(
        &self,
        view: &sourceview::View,
        messages: &[String],
        parts: &[String],
        start: &gtk::TextIter,
        end: &gtk::TextIter,
    ) {
        let imp = self.imp();
        let (x, y) = imp.pointer.get();
        let Some(area) = area(view, start, end).filter(|area| contains(area, x, y)) else {
            return;
        };
        let Some(origin) = (self.parent())
            .and_then(|editor| view.compute_point(&editor, &graphene::Point::zero()))
        else {
            return;
        };
        if messages.is_empty() && parts.is_empty() {
            return;
        }
        imp.area.set(Some(area));
        self.fill(view, messages, parts);
        #[expect(clippy::cast_possible_truncation, reason = "pixels fit")]
        let (x, y) = (origin.x() as i32, origin.y() as i32);
        self.set_pointing_to(Some(&gdk::Rectangle::new(
            area.x() + x,
            area.y() + y,
            area.width(),
            area.height(),
        )));
        self.cancel_leaving();
        self.popup();
    }

    /// Fills the hover with `messages`, then `parts`, which scroll once they
    /// take two fifths of `view`'s height, so that the hover fits above or
    /// below what it describes, as one that fits on neither side is not
    /// shown at all.
    fn fill(&self, view: &sourceview::View, messages: &[String], parts: &[String]) {
        let labels = gtk::Box::new(gtk::Orientation::Vertical, 0);
        for message in messages {
            labels.append(&label(message, false));
        }
        for part in parts {
            if labels.first_child().is_some() {
                labels.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
            }
            labels.append(&label(part, true));
        }
        let content = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vscrollbar_policy(gtk::PolicyType::Never)
            .overlay_scrolling(false)
            .propagate_natural_width(true)
            .propagate_natural_height(true)
            .max_content_height(view.height() * 2 / 5)
            .css_classes(["undershoot-top", "undershoot-bottom"])
            .child(&labels)
            .build();
        self.set_child(Some(&content));
        // Text taller than that scrolls, with a scrollbar and shadows at the
        // edges past which there is more, so that it shows at a glance. Its
        // height is as the hover styles it, which it now does.
        if labels.preferred_size().1.height() > content.max_content_height() {
            content.set_vscrollbar_policy(gtk::PolicyType::Automatic);
        }
    }

    /// Whether the hover shows what is at `x`, `y` in the view.
    fn shows(&self, x: f64, y: f64) -> bool {
        self.is_visible() && (self.imp().area.get()).is_some_and(|area| contains(&area, x, y))
    }

    /// Hides the hover once the grace is over, unless the pointer is back
    /// over it or what it describes by then.
    fn leave(&self) {
        let imp = self.imp();
        if !self.is_visible() || imp.leaving.borrow().is_some() {
            return;
        }
        let hover = self.downgrade();
        let leaving = glib::timeout_add_local_once(GRACE, move || {
            if let Some(hover) = hover.upgrade() {
                hover.imp().leaving.take();
                hover.popdown();
            }
        });
        imp.leaving.replace(Some(leaving));
    }

    fn cancel_leaving(&self) {
        if let Some(leaving) = self.imp().leaving.take() {
            leaving.remove();
        }
    }

    /// Gives up on showing what the pointer rests on, as it has moved on.
    fn stop_looking(&self) {
        let imp = self.imp();
        if let Some(resting) = imp.resting.take() {
            resting.remove();
        }
        if let Some(lookup) = imp.lookup.take() {
            lookup.abort();
        }
    }

    /// Hides the hover until the pointer next rests.
    fn dismiss(&self) {
        self.stop_looking();
        self.cancel_leaving();
        self.popdown();
    }

    /// Hides the hover as a key other than a modifier is pressed in it, and
    /// has the view, which gets the focus back, take the key, unless it is
    /// Escape.
    fn key_pressed(&self, keys: &gtk::EventControllerKey, key: gdk::Key) -> glib::Propagation {
        if is_modifier(keys) {
            return glib::Propagation::Proceed;
        }
        self.dismiss();
        let taken = key == gdk::Key::Escape
            || (self.imp().editor.upgrade()).is_some_and(|editor| keys.forward(&editor.view()));
        glib::Propagation::from(taken)
    }
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
        .margin_start(MARGIN)
        .margin_end(MARGIN)
        .margin_top(6)
        .margin_bottom(6)
        .build()
}

/// The run of characters around `iter`, on its line, of the kind of the one
/// at it: those of identifiers, spaces, or others.
fn word(iter: &gtk::TextIter) -> (gtk::TextIter, gtk::TextIter) {
    let kind = |c: char| (c.is_whitespace(), c.is_alphanumeric() || c == '_');
    let own = kind(iter.char());
    let unlike = move |c: char| kind(c) != own;
    let (mut line_start, mut line_end) = (*iter, *iter);
    line_start.set_line_offset(0);
    if !line_end.ends_line() {
        line_end.forward_to_line_end();
    }
    let (mut start, mut end) = (*iter, *iter);
    if start.backward_find_char(unlike, Some(&line_start)) {
        start.forward_char();
    }
    end.forward_find_char(unlike, Some(&line_end));
    (start, end)
}

/// Where the text from `start` to `end` shows in `view`, in its coordinates.
fn area(
    view: &sourceview::View,
    start: &gtk::TextIter,
    end: &gtk::TextIter,
) -> Option<gdk::Rectangle> {
    let (first, last) = (view.iter_location(start), view.iter_location(end));
    let area = if first.y() == last.y() {
        gdk::Rectangle::new(first.x(), first.y(), last.x() - first.x(), first.height())
    } else {
        first.union(&last)
    };
    let area = area.intersect(&view.visible_rect())?;
    let (x, y) = view.buffer_to_window_coords(gtk::TextWindowType::Widget, area.x(), area.y());
    Some(gdk::Rectangle::new(x, y, area.width(), area.height()))
}

/// Whether `area` holds the point at `x`, `y`.
fn contains(area: &gdk::Rectangle, x: f64, y: f64) -> bool {
    #[expect(clippy::cast_possible_truncation, reason = "pixels fit")]
    area.contains_point(x.floor() as i32, y.floor() as i32)
}

/// Whether the key `keys` hears pressed is a modifier, such as Ctrl.
fn is_modifier(keys: &gtk::EventControllerKey) -> bool {
    (keys.current_event()).is_some_and(|event| {
        (event.downcast_ref::<gdk::KeyEvent>()).is_some_and(gdk::KeyEvent::is_modifier)
    })
}
