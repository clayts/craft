//! How each kind of token that language servers pick out looks: as
//! `theme.json` in Craft's configuration folder says, read when Craft starts,
//! or else as Craft's own theme does, in light and in dark.
//!
//! A theme maps selectors to styles. A selector is a token type, or `*` for
//! any, then any modifiers, each after a dot, as in `variable.mutable` or
//! `*.deprecated`; several, separated by commas, share a style. A style can
//! give a `light` and a `dark` color, and say whether text is `bold`,
//! `italic`, `underline`d or `strikethrough`. Each of these comes from the
//! most specific rule that sets it, the one naming the most of what the
//! token is, and of those equally specific, the last.

use std::fmt;
use std::io;
use std::path::Path;
use std::sync::LazyLock;

use gtk::glib::translate::IntoGlib;
use gtk::prelude::*;
use gtk::{gdk, glib, pango};
use serde::de::{self, MapAccess, Visitor};
use serde::{Deserialize, Deserializer};

/// Craft's own theme.
const DEFAULT: &str = include_str!("theme.json");

/// The theme in use, and why `theme.json` is not it, if it is there but could
/// not be.
static CURRENT: LazyLock<(Theme, Option<String>)> = LazyLock::new(|| {
    let path = glib::user_config_dir().join("craft").join("theme.json");
    match read(&path) {
        Ok(theme) => (theme, None),
        Err(error) => (default(), Some(error)),
    }
});

/// A theme's rules, in the order written.
#[derive(Debug)]
pub struct Theme {
    rules: Vec<Rule>,
}

#[derive(Debug)]
struct Rule {
    /// The token type, or `None` for any.
    kind: Option<String>,
    modifiers: Vec<String>,
    style: Style,
}

/// How a rule styles text, in what it sets.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Style {
    light: Option<Color>,
    dark: Option<Color>,
    bold: Option<bool>,
    italic: Option<bool>,
    underline: Option<bool>,
    strikethrough: Option<bool>,
}

#[derive(Clone, Copy, Debug)]
struct Color(gdk::RGBA);

/// How a kind of token looks, light or dark, in what the theme sets.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Look {
    color: Option<gdk::RGBA>,
    bold: Option<bool>,
    italic: Option<bool>,
    underline: Option<bool>,
    strikethrough: Option<bool>,
}

impl Theme {
    pub fn parse(json: &str) -> serde_json::Result<Self> {
        serde_json::from_str(json)
    }

    /// How a token of type `kind` with `modifiers` looks, in dark or light.
    pub fn look(&self, kind: &str, modifiers: &[&str], dark: bool) -> Look {
        let mut rules: Vec<&Rule> = (self.rules.iter())
            .filter(|rule| rule.matches(kind, modifiers))
            .collect();
        // Stable, so that of equally specific rules, the later comes later.
        rules.sort_by_key(|rule| rule.specificity());
        (rules.into_iter()).fold(Look::default(), |look, rule| look.with(&rule.style, dark))
    }
}

impl Rule {
    fn new(selector: &str, style: Style) -> Result<Self, String> {
        let mut parts = selector.split('.');
        let kind = parts.next().unwrap_or_default();
        let modifiers: Vec<String> = parts.map(str::to_owned).collect();
        if kind.is_empty() || modifiers.iter().any(String::is_empty) {
            return Err(format!("“{selector}” is not a selector"));
        }
        Ok(Self {
            kind: (kind != "*").then(|| kind.to_owned()),
            modifiers,
            style,
        })
    }

    fn matches(&self, kind: &str, modifiers: &[&str]) -> bool {
        self.kind.as_ref().is_none_or(|own| own == kind)
            && (self.modifiers.iter()).all(|modifier| modifiers.contains(&modifier.as_str()))
    }

    /// How much of what a token is the rule names.
    fn specificity(&self) -> usize {
        usize::from(self.kind.is_some()) + self.modifiers.len()
    }
}

impl Look {
    pub fn is_plain(&self) -> bool {
        *self == Self::default()
    }

    /// A tag that gives text this look.
    pub fn tag(&self) -> gtk::TextTag {
        let tag = gtk::TextTag::new(None);
        if let Some(color) = &self.color {
            tag.set_foreground_rgba(Some(color));
        }
        if let Some(bold) = self.bold {
            let weight = if bold {
                pango::Weight::Bold
            } else {
                pango::Weight::Normal
            };
            tag.set_weight(weight.into_glib());
        }
        if let Some(italic) = self.italic {
            tag.set_style(if italic {
                pango::Style::Italic
            } else {
                pango::Style::Normal
            });
        }
        if let Some(underline) = self.underline {
            tag.set_underline(if underline {
                pango::Underline::Single
            } else {
                pango::Underline::None
            });
        }
        if let Some(strikethrough) = self.strikethrough {
            tag.set_strikethrough(strikethrough);
        }
        tag
    }

    /// This look, with what `style` sets, in dark or light, in place of its
    /// own.
    fn with(self, style: &Style, dark: bool) -> Self {
        let color = if dark { style.dark } else { style.light };
        Self {
            color: color.map_or(self.color, |Color(color)| Some(color)),
            bold: style.bold.or(self.bold),
            italic: style.italic.or(self.italic),
            underline: style.underline.or(self.underline),
            strikethrough: style.strikethrough.or(self.strikethrough),
        }
    }
}

impl<'de> Deserialize<'de> for Theme {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_map(Rules)
    }
}

/// Reads a theme's rules in the order written, which settles ties.
struct Rules;

impl<'de> Visitor<'de> for Rules {
    type Value = Theme;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("selectors and their styles")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Theme, A::Error> {
        let mut rules = Vec::new();
        while let Some((selectors, style)) = map.next_entry::<String, Style>()? {
            for selector in selectors.split(',') {
                let rule = Rule::new(selector.trim(), style.clone()).map_err(de::Error::custom)?;
                rules.push(rule);
            }
        }
        Ok(Theme { rules })
    }
}

impl<'de> Deserialize<'de> for Color {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        gdk::RGBA::parse(text.as_str())
            .map(Self)
            .map_err(|_| de::Error::custom(format!("“{text}” is not a color")))
    }
}

/// How a token of type `kind` with `modifiers` looks now, in light or dark as
/// the style is.
pub fn look(kind: &str, modifiers: &[&str]) -> Look {
    let dark = adw::StyleManager::default().is_dark();
    CURRENT.0.look(kind, modifiers, dark)
}

/// Why `theme.json` is not in use, if it is there but could not be.
pub fn error() -> Option<&'static str> {
    CURRENT.1.as_deref()
}

/// The theme in the file at `path`, or Craft's own if there is none there,
/// or else why the file cannot be used.
fn read(path: &Path) -> Result<Theme, String> {
    match std::fs::read_to_string(path) {
        Ok(json) => {
            Theme::parse(&json).map_err(|err| format!("Could not use {}\n{err}", path.display()))
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(default()),
        Err(err) => Err(format!("Could not read {}\n{err}", path.display())),
    }
}

fn default() -> Theme {
    Theme::parse(DEFAULT).expect("Craft's own theme is valid")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rgba(color: &str) -> Option<gdk::RGBA> {
        gdk::RGBA::parse(color).ok()
    }

    #[test]
    fn crafts_own_theme_is_valid() {
        let theme = default();
        let look = theme.look("struct", &["declaration"], true);
        assert_eq!(look.color, rgba("#5bc8af"));
        assert_eq!(look.bold, Some(true));
        assert!(theme.look("keyword", &[], false).is_plain());
    }

    #[test]
    fn specific_rules_win_and_later_ones_break_ties() {
        let theme = Theme::parse(
            r#"{
                "*.mutable": { "underline": true, "light": "red" },
                "variable": { "light": "blue", "dark": "navy" },
                "variable.mutable, *.public": { "light": "green" },
                "*.unsafe": { "light": "purple", "bold": true }
            }"#,
        )
        .unwrap();
        let variable = theme.look("variable", &[], false);
        assert_eq!((variable.color, variable.underline), (rgba("blue"), None));
        assert_eq!(theme.look("variable", &[], true).color, rgba("navy"));
        let mutable = theme.look("variable", &["mutable"], false);
        assert_eq!(
            (mutable.color, mutable.underline),
            (rgba("green"), Some(true))
        );
        // Of equally specific rules, the last wins.
        let both = theme.look("variable", &["public", "unsafe"], false);
        assert_eq!((both.color, both.bold), (rgba("purple"), Some(true)));
        assert_eq!(theme.look("function", &["mutable"], true).color, None);
    }

    #[test]
    fn mistakes_are_explained() {
        let error = |json| Theme::parse(json).unwrap_err().to_string();
        assert!(error(r#"{ "function": { "light": "bleu" } }"#).contains("“bleu” is not a color"));
        assert!(error(r#"{ "function": { "colour": "red" } }"#).contains("unknown field `colour`"));
        assert!(error(r#"{ "function..mutable": {} }"#).contains("is not a selector"));
        assert!(error(r#"{ "function, ": {} }"#).contains("“” is not a selector"));
    }
}
