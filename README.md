# Craft

A minimal code editor for GNOME, written in Rust with GTK 4, libadwaita and
GtkSourceView.

Craft has a project file tree, tabs, find and replace, and language server
support, and nothing else: no settings, git, AI, plugins, terminal or run
commands. Find and replace, semantic highlighting and symbol search are not
built yet.

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

### With Nix

The flake builds against nixos-unstable:

```sh
nix run        # build and run Craft
nix build      # build it into ./result
nix develop    # a shell with Rust, the libraries, Clippy, rustfmt and rust-analyzer
```

Inside `nix develop`, `cargo build` and `cargo run` work as usual. `nix fmt`
formats the Nix files.

## Usage

```sh
craft [FOLDER | FILE]...
```

Each folder opens as a project in a window of its own. Files open as tabs in
the window of the last folder given, or else in a window of their own. A file
that does not exist yet is created when you first save it. Each run of Craft is
separate, with its own windows and the environment it was started in. Opening a
folder from a window that has one already opens it in a new window.

## Keyboard shortcuts

| Shortcut                             | Action           |
| ------------------------------------ | ---------------- |
| Ctrl+O                               | Open folder      |
| Ctrl+S                               | Save             |
| Ctrl+W                               | Close tab        |
| Ctrl+Q                               | Quit             |
| F9                                   | Toggle sidebar   |
| F12                                  | Go to definition |
| Ctrl+Space                           | Complete         |
| Ctrl+Tab, Ctrl+Page Up/Down          | Switch tab       |
| Alt+1…9                              | Go to tab        |
| Ctrl+Shift+Page Up/Down              | Move tab         |

## Language servers

Craft starts a language server for each language you open files in, using the
first of these it finds on the `PATH`:

| Language               | Servers                                                  |
| ---------------------- | -------------------------------------------------------- |
| Rust                   | `rust-analyzer`                                          |
| C, C++, Objective-C    | `clangd`                                                 |
| Go                     | `gopls`                                                  |
| Python                 | `basedpyright-langserver`, `pyright-langserver`, `pylsp` |
| JavaScript, TypeScript | `typescript-language-server`                             |
| Nix                    | `nixd`, `nil`                                            |
| Shell                  | `bash-language-server`                                   |
| Zig                    | `zls`                                                    |
| Lua                    | `lua-language-server`                                    |
| TOML                   | `taplo`                                                  |
| JSON                   | `vscode-json-language-server`                            |
| YAML                   | `yaml-language-server`                                   |
| HTML                   | `vscode-html-language-server`                            |
| CSS                    | `vscode-css-language-server`                             |

Servers run for the open folder, on its `PATH`: Craft's own, changed as direnv
changes a shell's on entering the folder, so servers can come from the
project's development shell. If the `.envrc` is blocked, Craft says so: run
`direnv allow`, then open a file to start its server.

A file opened on its own gets what its server can offer without a folder, in
Craft's own environment: most servers work file by file, and those that need a
folder, such as rust-analyzer, say so.

Errors, warnings and notes are underlined, and stay in place as you type.
Hovering shows their messages and the server's description of what is under
the pointer.

Completions appear as you type, or on Ctrl+Space, best matches first. Enter or
Tab inserts the one selected, adding any import it needs, and Tab then moves
through its fields, such as a function's arguments.

Servers that can say what each piece of code is, such as a function, a type or
a mutable variable, highlight it as such, as the [theme](#theme) says, keeping
up as you type.

A button in the header bar shows how the tools a window runs, its language
servers and direnv, are faring: a spinner while any is at work, otherwise a
warning sign if any has failed since it last started, and otherwise a tick. It
lists the tools, each with its own sign and the work it has under way, and each
opens onto its log: starting, failing to start or crashing, its program missing
from the `PATH`, and all it said, from errors such as failing to load the
project to notes. The log keeps its latest entry in view, unless scrolled away
from it. A server that crashes after running for at least 30 seconds restarts
by itself; one that crashes sooner restarts when you next open a file in its
language.

## Theme

Semantic highlighting follows `theme.json` in Craft's configuration folder,
normally `~/.config/craft/theme.json`, or else Craft's own theme,
[`src/theme.json`](src/theme.json), a good start for your own. A change shows as
soon as the file is saved. The file maps selectors to styles:

```json
{
  "function, method": { "light": "#1c71d8", "dark": "#62a0ea" },
  "struct, enum": { "light": "#218787", "dark": "#5bc8af", "bold": true },
  "*.mutable": { "underline": true },
  "*.deprecated": { "strikethrough": true }
}
```

A selector names a token type, or `*` for any, then any modifiers, each after a
dot; several, separated by commas, share a style. Types and modifiers are those
servers send: the protocol's own, such as `function`, `variable` and
`readonly`, and each server's, such as rust-analyzer's `lifetime` and `unsafe`.
A style can give `light` and `dark` colors, as names, hex or `rgb()`, and set
`bold`, `italic`, `underline` and `strikethrough`. Each comes from the most
specific rule that sets it, the one naming the most of what the token is, and
of those equally specific, the last. What no rule styles keeps its syntax
highlighting.

If the file cannot be used, the status button says why, and the theme in use
stays.

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

| File               | Contents                                                                                  |
| ------------------ | ----------------------------------------------------------------------------------------- |
| `src/main.rs`      | Application: startup, and routing folders and files                                       |
| `src/window.rs`    | Project window: layout, tabs, actions, close prompts                                      |
| `src/editor.rs`    | One open file: source view, loading and saving                                            |
| `src/file_tree.rs` | Sidebar tree: loaded lazily, updated live                                                 |
| `src/theme.rs`     | Theme: `theme.json`, read and kept up with                                                |
| `src/theme.json`   | Craft's own theme                                                                         |
| `src/lsp/`         | Language servers: environments, processes, files, hover, completion, highlighting, status |
| `package.nix`      | Nix package                                                                               |
| `flake.nix`        | Nix flake: package, development shell and formatter                                       |
