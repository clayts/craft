//! A tool's log, read in a tab as if it were a text file.

use std::cell::Cell;
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;

/// A view of `log` that keeps the end of it in view while the end is in
/// view: when it opens, and as entries come.
pub fn new(log: &sourceview::Buffer) -> gtk::ScrolledWindow {
    let view = sourceview::View::builder()
        .buffer(log)
        .editable(false)
        .monospace(true)
        .left_margin(12)
        .top_margin(6)
        .bottom_margin(6)
        .build();
    let scroller = gtk::ScrolledWindow::builder().child(&view).build();
    let adjustment = scroller.vadjustment();
    let following = Rc::new(Cell::new(true));
    adjustment.connect_value_changed(glib::clone!(
        #[strong]
        following,
        move |adjustment| {
            following.set(adjustment.value() >= adjustment.upper() - adjustment.page_size());
        }
    ));
    adjustment.connect_changed(move |adjustment| {
        if following.get() {
            adjustment.set_value(adjustment.upper() - adjustment.page_size());
        }
    });
    scroller
}

/// Whether `page` shows `log`.
pub fn shows(page: &adw::TabPage, log: &sourceview::Buffer) -> bool {
    let view = page
        .child()
        .downcast::<gtk::ScrolledWindow>()
        .ok()
        .and_then(|scroller| scroller.child())
        .and_downcast::<gtk::TextView>();
    view.is_some_and(|view| view.buffer() == *log.upcast_ref::<gtk::TextBuffer>())
}
