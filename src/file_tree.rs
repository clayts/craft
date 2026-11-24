//! The project sidebar: a lazily loaded, live-updating tree of files.

use adw::prelude::*;
use gtk::{gio, glib, pango};

/// The file attributes the tree reads. `standard::name` is required for
/// directory lists to resolve their children.
const ATTRIBUTES: &str = "standard::name,standard::display-name,standard::type";

/// Version control metadata, never shown.
const HIDDEN: [&str; 3] = [".git", ".hg", ".svn"];

/// A list view of the files under `root`, which calls `on_open` with each
/// regular file the user activates.
pub fn new(root: &gio::File, on_open: impl Fn(gio::File) + 'static) -> gtk::ListView {
    let filter =
        gtk::CustomFilter::new(|item| !HIDDEN.contains(&info(item).display_name().as_str()));
    let sorter = gtk::MultiSorter::new();
    sorter.append(gtk::NumericSorter::new(Some(info_expression(|info| {
        info.file_type() != gio::FileType::Directory
    }))));
    sorter.append(
        gtk::StringSorter::builder()
            .expression(info_expression(gio::FileInfo::display_name))
            .collation(gtk::Collation::Filename)
            .build(),
    );

    let root = directory(root, &filter, &sorter);
    let tree = gtk::TreeListModel::new(root, false, false, move |item| {
        let info = info(item);
        (info.file_type() == gio::FileType::Directory)
            .then(|| directory(&file(info), &filter, &sorter))
    });

    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(|_, item| {
        let label = gtk::Label::builder()
            .xalign(0.0)
            .ellipsize(pango::EllipsizeMode::End)
            .build();
        list_item(item).set_child(Some(&gtk::TreeExpander::builder().child(&label).build()));
    });
    factory.connect_bind(|_, item| {
        let item = list_item(item);
        let row = item
            .item()
            .and_downcast::<gtk::TreeListRow>()
            .expect("tree items are rows");
        let expander = expander(item);
        let label = expander
            .child()
            .and_downcast::<gtk::Label>()
            .expect("expanders hold labels");
        label.set_label(&info(&row.item().expect("rows hold items")).display_name());
        expander.set_list_row(Some(&row));
    });
    factory.connect_unbind(|_, item| expander(list_item(item)).set_list_row(None));

    let view = gtk::ListView::builder()
        .model(
            &gtk::SingleSelection::builder()
                .model(&tree)
                .autoselect(false)
                .build(),
        )
        .factory(&factory)
        .css_classes(["navigation-sidebar"])
        .build();
    view.connect_activate(move |_, position| {
        let Some(row) = tree.row(position) else {
            return;
        };
        let item = row.item().expect("rows hold items");
        let info = info(&item);
        match info.file_type() {
            gio::FileType::Directory => row.set_expanded(!row.is_expanded()),
            gio::FileType::Regular => on_open(file(info)),
            _ => {}
        }
    });
    view
}

/// The filtered, sorted, monitored contents of `dir`.
fn directory(
    dir: &gio::File,
    filter: &gtk::CustomFilter,
    sorter: &gtk::MultiSorter,
) -> gio::ListModel {
    let list = gtk::DirectoryList::new(Some(ATTRIBUTES), gio::File::NONE);
    // GTK builds and drops a child model for every directory row it shows,
    // only to learn that the row is expandable. Starting the load from idle
    // means only models that are kept, for expanded rows, touch the disk.
    let weak = list.downgrade();
    let dir = dir.clone();
    glib::idle_add_local_once(move || {
        if let Some(list) = weak.upgrade() {
            list.set_file(Some(&dir));
        }
    });
    let filtered = gtk::FilterListModel::new(Some(list), Some(filter.clone()));
    gtk::SortListModel::new(Some(filtered), Some(sorter.clone())).upcast()
}

/// An expression evaluating `f` on the file info it is applied to.
fn info_expression<T: glib::value::ValueType>(
    f: impl Fn(&gio::FileInfo) -> T + 'static,
) -> gtk::ClosureExpression {
    gtk::ClosureExpression::with_callback(gtk::Expression::NONE, move |values| {
        f(&values[0].get().expect("expressions apply to file infos"))
    })
}

fn info(item: &glib::Object) -> &gio::FileInfo {
    item.downcast_ref()
        .expect("directory lists hold file infos")
}

fn file(info: &gio::FileInfo) -> gio::File {
    info.attribute_object("standard::file")
        .and_downcast()
        .expect("directory lists set standard::file")
}

fn list_item(item: &glib::Object) -> &gtk::ListItem {
    item.downcast_ref().expect("list views use list items")
}

fn expander(item: &gtk::ListItem) -> gtk::TreeExpander {
    item.child()
        .and_downcast()
        .expect("list items hold expanders")
}
