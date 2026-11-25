//! The environment language servers run in: Craft's own, changed as direnv
//! would change it for the project, so that servers come from the project's
//! development shell where it has one.

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::fmt::Display;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use gtk::prelude::*;
use gtk::{gio, glib};

use crate::display_name;

#[derive(Debug, Default, PartialEq)]
pub struct Environment {
    /// Variables to set, or to unset where `None`.
    changes: HashMap<String, Option<String>>,
    /// Whether direnv failed to give the project's environment.
    pub failed: bool,
}

impl Environment {
    /// The environment of the project at `root`. Without direnv or an
    /// `.envrc`, that is Craft's own. Errors describe why direnv failed.
    pub async fn load(root: &gio::File) -> Result<Self, String> {
        let (Some(dir), Some(_)) = (root.path(), glib::find_program_in_path("direnv")) else {
            return Ok(Self::default());
        };
        let name = display_name(root);
        let fail = |detail: &dyn Display| format!("Could not load {name}’s environment:\n{detail}");
        let launcher = gio::SubprocessLauncher::new(
            gio::SubprocessFlags::STDOUT_PIPE | gio::SubprocessFlags::STDERR_PIPE,
        );
        launcher.set_cwd(&dir);
        let argv = ["direnv", "export", "json"].map(OsStr::new);
        let process = launcher.spawn(&argv).map_err(|err| fail(&err))?;
        let (stdout, stderr) = process
            .communicate_utf8_future(None)
            .await
            .map_err(|err| fail(&err))?;
        if !process.is_successful() {
            let stderr = stderr.unwrap_or_default();
            return Err(if stderr.contains(" is blocked") {
                format!("{name}’s .envrc is blocked; run “direnv allow” to use it")
            } else {
                fail(&plain(&stderr))
            });
        }
        Self::parse(stdout.as_deref().unwrap_or_default()).map_err(|err| fail(&err))
    }

    /// Craft's own environment, standing in for one direnv failed to give.
    pub fn failed() -> Self {
        Self {
            failed: true,
            ..Self::default()
        }
    }

    /// Reads the output of `direnv export json`, which is empty when there is
    /// nothing to change.
    fn parse(json: &str) -> serde_json::Result<Self> {
        if json.trim().is_empty() {
            return Ok(Self::default());
        }
        Ok(Self {
            changes: serde_json::from_str(json)?,
            failed: false,
        })
    }

    /// The first of `commands` whose program is on the environment's `PATH`,
    /// with the program's path.
    pub fn find<'a>(&self, commands: &'a [&'a [&'a str]]) -> Option<(PathBuf, &'a [&'a str])> {
        let path = match self.changes.get("PATH") {
            Some(path) => OsString::from(path.as_ref()?),
            None => std::env::var_os("PATH")?,
        };
        commands.iter().find_map(|&command| {
            let name = command.first()?;
            let program = std::env::split_paths(&path)
                .filter(|dir| dir.is_absolute())
                .map(|dir| dir.join(name))
                .find(|file| is_executable(file))?;
            Some((program, command))
        })
    }

    pub fn apply(&self, command: &mut Command) {
        for (name, value) in &self.changes {
            match value {
                Some(value) => command.env(name, value),
                None => command.env_remove(name),
            };
        }
    }
}

/// `text` without the escape sequences that color it on a terminal, which
/// direnv writes regardless.
fn plain(text: &str) -> String {
    let mut plain = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            plain.push(c);
        } else if chars.next() == Some('[') {
            // Parameters, up to the final letter.
            chars.by_ref().find(|c| ('@'..='~').contains(c));
        }
    }
    plain
}

fn is_executable(file: &Path) -> bool {
    file.metadata()
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn parses_changes_and_their_absence() {
        assert_eq!(Environment::parse("").unwrap(), Environment::default());
        let environment = Environment::parse(r#"{"A": "1", "B": null}"#).unwrap();
        assert_eq!(environment.changes["A"].as_deref(), Some("1"));
        assert_eq!(environment.changes["B"], None);
        assert!(!environment.failed);
    }

    #[test]
    fn colors_are_stripped() {
        assert_eq!(
            plain("\x1b[31mdirenv: error .envrc\x1b[0m\n\x1b[1;33mok"),
            "direnv: error .envrc\nok"
        );
        assert_eq!(plain("plain"), "plain");
    }

    #[test]
    fn applies_changes_to_commands() {
        let environment = Environment::parse(r#"{"A": "1", "B": null}"#).unwrap();
        let mut command = Command::new("true");
        environment.apply(&mut command);
        let mut envs: Vec<_> = command.get_envs().collect();
        envs.sort();
        assert_eq!(
            envs,
            [
                (OsStr::new("A"), Some(OsStr::new("1"))),
                (OsStr::new("B"), None)
            ]
        );
    }

    #[test]
    fn finds_executables_on_the_changed_path() {
        let dir = std::env::temp_dir().join(format!("craft-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let (server, notes) = (dir.join("server"), dir.join("notes"));
        fs::write(&server, "").unwrap();
        fs::write(&notes, "").unwrap();
        fs::set_permissions(&server, fs::Permissions::from_mode(0o755)).unwrap();

        let json = serde_json::json!({ "PATH": format!("relative:{}", dir.display()) });
        let environment = Environment::parse(&json.to_string()).unwrap();
        let commands: &[&[&str]] = &[&["missing"], &["notes"], &["server", "--stdio"]];
        assert_eq!(
            environment.find(commands),
            Some((server, &["server", "--stdio"][..]))
        );
        assert_eq!(environment.find(&[&["notes"]]), None);
        fs::remove_dir_all(dir).unwrap();
    }
}
