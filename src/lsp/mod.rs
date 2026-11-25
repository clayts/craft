//! Language server support: a window runs a server per language and project,
//! found on the project's `PATH`, and connects each open file to its server.

mod client;
mod completion;
mod document;
mod environment;
mod hover;
mod position;
mod progress;

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::Path;
use std::rc::Rc;

use futures_util::FutureExt;
use futures_util::future::{LocalBoxFuture, Shared};
use gtk::prelude::*;
use gtk::{gio, glib};

use client::Client;
pub use document::Document;
use environment::Environment;
pub use position::Encoding;
use position::Shift;
use progress::Progress;

use crate::editor::Editor;

/// Tells the user of a problem.
type Report = Rc<dyn Fn(&str)>;

/// A project's environment, loaded once for all of its servers.
type SharedEnvironment = Shared<LocalBoxFuture<'static, Rc<Environment>>>;

/// A language server: the command lines to try, the first found on the
/// project's `PATH` winning, the `GtkSourceView` languages it serves with the
/// LSP identifier of each, and the files marking a project's root folder.
struct Server {
    commands: &'static [&'static [&'static str]],
    languages: &'static [(&'static str, &'static str)],
    markers: &'static [&'static str],
}

const SERVERS: &[Server] = &[
    Server {
        commands: &[&["rust-analyzer"]],
        languages: &[("rust", "rust")],
        markers: &["Cargo.toml", "rust-project.json"],
    },
    Server {
        commands: &[&["clangd"]],
        languages: &[
            ("c", "c"),
            ("chdr", "c"),
            ("cpp", "cpp"),
            ("cpphdr", "cpp"),
            ("objc", "objective-c"),
        ],
        markers: &["compile_commands.json", "compile_flags.txt", ".clangd"],
    },
    Server {
        commands: &[&["gopls"]],
        languages: &[("go", "go")],
        markers: &["go.work", "go.mod"],
    },
    Server {
        commands: &[
            &["basedpyright-langserver", "--stdio"],
            &["pyright-langserver", "--stdio"],
            &["pylsp"],
        ],
        languages: &[("python3", "python"), ("python", "python")],
        markers: &[
            "pyproject.toml",
            "setup.py",
            "setup.cfg",
            "pyrightconfig.json",
        ],
    },
    Server {
        commands: &[&["typescript-language-server", "--stdio"]],
        languages: &[
            ("js", "javascript"),
            ("jsx", "javascriptreact"),
            ("typescript", "typescript"),
            ("typescript-jsx", "typescriptreact"),
        ],
        markers: &["tsconfig.json", "jsconfig.json", "package.json"],
    },
    Server {
        commands: &[&["nixd"], &["nil"]],
        languages: &[("nix", "nix")],
        markers: &["flake.nix"],
    },
    Server {
        commands: &[&["bash-language-server", "start"]],
        languages: &[("sh", "shellscript")],
        markers: &[],
    },
    Server {
        commands: &[&["zls"]],
        languages: &[("zig", "zig")],
        markers: &["build.zig"],
    },
    Server {
        commands: &[&["lua-language-server"]],
        languages: &[("lua", "lua")],
        markers: &[".luarc.json", ".luarc.jsonc"],
    },
    Server {
        commands: &[&["taplo", "lsp", "stdio"]],
        languages: &[("toml", "toml")],
        markers: &[".taplo.toml", "taplo.toml"],
    },
    Server {
        commands: &[&["vscode-json-language-server", "--stdio"]],
        languages: &[("json", "json")],
        markers: &["package.json"],
    },
    Server {
        commands: &[&["yaml-language-server", "--stdio"]],
        languages: &[("yaml", "yaml")],
        markers: &[],
    },
    Server {
        commands: &[&["vscode-html-language-server", "--stdio"]],
        languages: &[("html", "html")],
        markers: &["package.json"],
    },
    Server {
        commands: &[&["vscode-css-language-server", "--stdio"]],
        languages: &[("css", "css")],
        markers: &["package.json"],
    },
];

/// Folders marking a repository's root.
const REPOSITORIES: &[&str] = &[".git", ".hg", ".svn", ".jj"];

/// The server for the `GtkSourceView` language `language`, by index, with the
/// language's LSP identifier.
fn server_for(language: &str) -> Option<(usize, &'static str)> {
    SERVERS.iter().enumerate().find_map(|(index, server)| {
        let &(_, id) = server
            .languages
            .iter()
            .find(|&&(name, _)| name == language)?;
        Some((index, id))
    })
}

/// A window's language servers, by server and project root.
pub struct Servers {
    clients: RefCell<HashMap<(usize, glib::GString), Rc<Client>>>,
    environments: RefCell<HashMap<glib::GString, SharedEnvironment>>,
    report: Report,
    progress: Rc<Progress>,
}

impl Servers {
    /// Servers that tell `report` of the problems they meet.
    pub fn new(report: impl Fn(&str) + 'static) -> Self {
        Self {
            clients: RefCell::default(),
            environments: RefCell::default(),
            report: Rc::new(report),
            progress: Progress::new(),
        }
    }

    /// Shows what the servers are busy with.
    pub fn indicator(&self) -> &gtk::Revealer {
        self.progress.widget()
    }

    /// Connects `editor` to the server for its language, starting it if needed
    /// for the project the file belongs to, as found in the window's
    /// `project`, or for the project of `origin`, whose definition led to it:
    /// a definition outside the project, as in a dependency, still belongs
    /// with the server that found it.
    pub fn attach(&self, editor: &Editor, project: Option<&gio::File>, origin: Option<&Document>) {
        let Some((index, language)) = editor.language().and_then(|name| server_for(&name)) else {
            return;
        };
        let server = &SERVERS[index];
        let root = match origin {
            Some(origin) => origin.client().root().clone(),
            None => match project_root(&editor.file(), project, server.markers) {
                Some(root) => root,
                None => return,
            },
        };
        let key = (index, root.uri());
        let running = self
            .clients
            .borrow()
            .get(&key)
            .filter(|client| !client.is_stopped())
            .cloned();
        let client = if let Some(client) = running {
            client
        } else {
            let environment = self.environment(&root);
            // Once the environment is known, a missing server is too.
            if environment
                .peek()
                .is_some_and(|environment| environment.find(server.commands).is_none())
            {
                return;
            }
            let client = Client::start(
                server,
                &root,
                environment,
                Rc::clone(&self.report),
                Rc::clone(&self.progress),
            );
            let stopped = self.clients.borrow_mut().insert(key, Rc::clone(&client));
            // The new server takes over the stopped one's files.
            for document in stopped.iter().flat_map(|stopped| stopped.documents()) {
                if let Some(editor) = document.editor() {
                    let document = Document::new(&client, &editor, document.language());
                    editor.set_document(Some(document));
                }
            }
            client
        };
        if !editor
            .document()
            .is_some_and(|document| Rc::ptr_eq(document.client(), &client))
        {
            editor.set_document(Some(Document::new(&client, editor, language)));
        }
    }

    /// Moves the files of `editors` in `project`, which their window now
    /// shows, to the servers for the projects they belong to there, and shuts
    /// down the servers they leave.
    pub fn move_to(&self, project: &gio::File, editors: impl IntoIterator<Item = Editor>) {
        let mut left: Vec<Rc<Client>> = Vec::new();
        for editor in editors {
            if !editor.file().has_prefix(project) {
                continue;
            }
            if let Some(document) = editor.document() {
                left.push(Rc::clone(document.client()));
            }
            self.attach(&editor, Some(project), None);
        }
        let idle: Vec<_> = self
            .clients
            .borrow_mut()
            .extract_if(|_, client| {
                !client.has_documents() && left.iter().any(|left| Rc::ptr_eq(left, client))
            })
            .map(|(_, client)| client)
            .collect();
        for client in idle {
            shut_down(client);
        }
    }

    /// The environment of the project at `root`, loaded once, or again after
    /// failing, in case the cause has been fixed.
    fn environment(&self, root: &gio::File) -> SharedEnvironment {
        let key = root.uri();
        if let Some(environment) = self.environments.borrow().get(&key)
            && !environment
                .peek()
                .is_some_and(|environment| environment.failed)
        {
            return environment.clone();
        }
        let (root, report) = (root.clone(), Rc::clone(&self.report));
        let environment = async move {
            Rc::new(Environment::load(&root).await.unwrap_or_else(|message| {
                report(&message);
                Environment::failed()
            }))
        }
        .boxed_local()
        .shared();
        self.environments
            .borrow_mut()
            .insert(key, environment.clone());
        environment
    }

    /// Shuts every server down, keeping the application running until they
    /// have exited.
    pub fn shutdown(&self) {
        self.environments.take();
        for client in self.clients.take().into_values() {
            shut_down(client);
        }
    }
}

/// The root folder of the project `file` belongs to, for a server whose
/// projects hold one of `markers`. See [`find_root`].
fn project_root(
    file: &gio::File,
    project: Option<&gio::File>,
    markers: &[&str],
) -> Option<gio::File> {
    let Some(dir) = file
        .path()
        .and_then(|path| path.parent().map(Path::to_owned))
    else {
        // Remote files leave no markers to look for.
        return project.cloned().or_else(|| file.parent());
    };
    let project = project.and_then(gio::File::path);
    Some(gio::File::for_path(find_root(
        &dir,
        project.as_deref(),
        markers,
    )))
}

/// The root folder of the project that the folder `dir` belongs to: in the
/// window's `project` if `dir` is in it, or else in `dir`'s repository, the
/// outermost folder holding one of `markers`, or else that project or
/// repository. Outside both, the nearest folder holding one, or else `dir`.
fn find_root<'a>(dir: &'a Path, project: Option<&'a Path>, markers: &[&str]) -> &'a Path {
    let holds = |dir: &Path, names: &[&str]| names.iter().any(|name| dir.join(name).exists());
    let bound = project
        .filter(|project| dir.starts_with(project))
        .or_else(|| dir.ancestors().find(|dir| holds(dir, REPOSITORIES)));
    match bound {
        Some(bound) => dir
            .ancestors()
            .take_while(|dir| dir.starts_with(bound))
            .filter(|dir| holds(dir, markers))
            .last()
            .unwrap_or(bound),
        None => dir
            .ancestors()
            .find(|dir| holds(dir, markers))
            .unwrap_or(dir),
    }
}

/// Shuts `client`'s server down, keeping the application running until it
/// has exited.
fn shut_down(client: Rc<Client>) {
    let hold = gio::Application::default().map(|application| application.hold());
    glib::spawn_future_local(async move {
        client.shutdown().await;
        drop(hold);
    });
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use super::find_root;

    const MARKERS: &[&str] = &["Cargo.toml"];

    /// Folders under a fresh temporary one, those with a `+` holding a marker
    /// and those with a `*` a repository.
    fn tree(name: &str, dirs: &[&str]) -> PathBuf {
        let base = std::env::temp_dir().join(format!("craft-{name}-{}", std::process::id()));
        for dir in dirs {
            let path = base.join(dir.trim_end_matches(['+', '*']));
            fs::create_dir_all(&path).unwrap();
            if dir.ends_with('+') {
                fs::write(path.join("Cargo.toml"), "").unwrap();
            }
            if dir.ends_with('*') {
                fs::create_dir(path.join(".git")).unwrap();
            }
        }
        base
    }

    #[test]
    fn roots_are_the_outermost_marked_folders_in_a_repository() {
        let base = tree(
            "repo",
            &["repo*", "repo+", "repo/crates/a+", "repo/crates/a/src"],
        );
        let src = base.join("repo/crates/a/src");
        assert_eq!(find_root(&src, None, MARKERS), base.join("repo"));
        // In a project, the outermost marked folder there.
        let project = base.join("repo/crates/a");
        assert_eq!(find_root(&src, Some(&project), MARKERS), project);
        // Without markers, the repository itself.
        assert_eq!(find_root(&src, None, &[]), base.join("repo"));
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn roots_are_the_nearest_marked_folders_elsewhere() {
        let base = tree("loose", &["a+", "a/b+", "a/b/src", "c"]);
        let src = base.join("a/b/src");
        assert_eq!(find_root(&src, None, MARKERS), base.join("a/b"));
        // Outside the project, as if it were not there.
        let project = base.join("c");
        assert_eq!(find_root(&src, Some(&project), MARKERS), base.join("a/b"));
        assert_eq!(find_root(&project, None, MARKERS), project);
        fs::remove_dir_all(base).unwrap();
    }
}
