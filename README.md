# Craft

A minimal code editor for GNOME, written in Rust with GTK 4, libadwaita and
GtkSourceView.

Craft has a project file tree, tabs, find and replace, and language server
support, and nothing else: no settings, git, AI, plugins, terminal or run
commands. Find and replace and language server support are not built yet.

## Building

You need Rust 1.92 or newer, plus GTK 4.14, libadwaita 1.5 and
GtkSourceView 5.12 or newer with their development files:

| Distribution  | Packages                                                     |
| ------------- | ------------------------------------------------------------ |
| Fedora        | `gtk4-devel libadwaita-devel gtksourceview5-devel`           |
| Debian/Ubuntu | `libgtk-4-dev libadwaita-1-dev libgtksourceview-5-dev`       |
| Arch          | `gtk4 libadwaita gtksourceview5`                             |

Then build and install to `~/.cargo/bin`:

```sh
cargo install --path .
```

## Usage

```sh
craft [FOLDER | FILE]...
```

Each folder opens as a project in its own window. Files open as tabs, in the
window of the last folder given, or else the active window. A file that does
not exist yet is created when you first save it. Craft runs as a single
instance, so running it again opens into the windows already on screen.

## Keyboard shortcuts

| Shortcut                             | Action           |
| ------------------------------------ | ---------------- |
| Ctrl+O                               | Open folder      |
| Ctrl+S                               | Save             |
| Ctrl+W                               | Close tab        |
| Ctrl+Q                               | Quit             |
| F9                                   | Toggle sidebar   |
| Ctrl+Tab, Ctrl+Page Up/Down          | Switch tab       |
| Alt+1…9                              | Go to tab        |
| Ctrl+Shift+Page Up/Down              | Move tab         |

## Behaviour

- **Files:** Craft edits UTF-8 text. It saves byte for byte, apart from line
  endings: a file whose first line ends in CRLF keeps CRLF on every line,
  including lines you add.
- **Safety:** saves are atomic. If another program changed a file since Craft
  read it, Craft asks before overwriting. Closing a tab or window with unsaved
  changes asks whether to save them.
- **File tree:** sorts folders first, in natural order (`file2` before
  `file10`), and hides `.git`, `.hg` and `.svn`. It updates as files change on
  disk, and only reads a folder once you expand it.
- **Indentation:** Tab inserts four spaces, except in Go and Makefiles, which
  use tabs.
- **Appearance:** follows the system's light or dark style.

## Source

| File               | Contents                                               |
| ------------------ | ------------------------------------------------------ |
| `src/main.rs`      | Application: startup, and routing folders and files    |
| `src/window.rs`    | Project window: layout, tabs, actions, close prompts   |
| `src/editor.rs`    | One open file: source view, loading and saving         |
| `src/file_tree.rs` | Sidebar tree: loaded lazily, updated live              |
