//! Projects opened lately, as GTK's list of recently used files keeps them,
//! which a window offers to open while it has none.

use std::cell::Cell;

use adw::prelude::*;
use gtk::{gio, glib};

use crate::display_name;

/// The name the list gives Craft, which opened the projects.
const APP: &str = "Craft";

/// The type the list gives folders.
const FOLDER: &str = "inode/directory";

/// How many projects a window offers.
const SHOWN: usize = 8;

/// Notes that `folder` was opened as a project.
pub fn add(folder: &gio::File) {
    let exec = format!("{} %u", glib::prgname().unwrap_or_default());
    let data = gtk::RecentData::new(None, None, FOLDER, APP, &exec, &[], false);
    gtk::RecentManager::default().add_full(&folder.uri(), &data);
}

/// A list of the projects opened lately that are still there, latest first,
/// which calls `open` with the one chosen and keeps up as others are opened.
/// It hides while there are none.
pub fn list(open: impl Fn(gio::File) + 'static) -> gtk::ListBox {
    let uris = gtk::StringList::new(&[]);
    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build();
    list.bind_model(Some(&uris), row);
    list.connect_row_activated(glib::clone!(
        #[weak]
        uris,
        move |_, row| {
            let index = u32::try_from(row.index()).expect("rows are in the list");
            if let Some(uri) = uris.string(index) {
                open(gio::File::for_uri(&uri));
            }
        }
    ));
    let manager = gtk::RecentManager::default();
    let update = glib::clone!(
        #[weak]
        list,
        move |manager: &gtk::RecentManager| {
            let projects = projects(manager);
            let projects: Vec<&str> = projects.iter().map(glib::GString::as_str).collect();
            uris.splice(0, uris.n_items(), &projects);
            list.set_visible(!projects.is_empty());
        }
    );
    update(&manager);
    let changed = Cell::new(Some(manager.connect_changed(update)));
    list.connect_destroy(move |_| {
        if let Some(changed) = changed.take() {
            manager.disconnect(changed);
        }
    });
    list
}

/// The URIs of the projects opened lately that are still there, latest first.
fn projects(manager: &gtk::RecentManager) -> Vec<glib::GString> {
    let mut projects: Vec<gtk::RecentInfo> = (manager.items().into_iter())
        .filter(|info| info.mime_type() == FOLDER && info.has_application(APP) && info.exists())
        .collect();
    projects.sort_by_key(|info| std::cmp::Reverse(info.modified()));
    (projects.into_iter())
        .take(SHOWN)
        .map(|info| info.uri())
        .collect()
}

/// A row for the project at `uri`: its name, and where it is.
fn row(uri: &glib::Object) -> gtk::Widget {
    let uri = (uri.downcast_ref::<gtk::StringObject>())
        .expect("the list holds URIs")
        .string();
    let folder = gio::File::for_uri(&uri);
    let row = adw::ActionRow::builder()
        .title(display_name(&folder))
        .subtitle(location(&folder))
        .use_markup(false)
        .activatable(true)
        .build();
    row.add_prefix(&gtk::Image::from_icon_name("folder-symbolic"));
    row.upcast()
}

/// Where `folder` is, as a path, with `~` for the home folder.
fn location(folder: &gio::File) -> String {
    let path = folder.parse_name().to_string();
    let home = glib::home_dir();
    match path.strip_prefix(&*home.to_string_lossy()) {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => format!("~{rest}"),
        _ => path,
    }
}
