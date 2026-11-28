//! Code in descriptions as Pango markup, highlighted as a source view
//! highlights it.

use gtk::prelude::*;
use gtk::{gdk, glib, pango};
use sourceview::prelude::*;

/// `code`, but for the line ends and spaces after it, as Pango markup in a
/// monospace font, coloured as `scheme` colours `language`, if it has one.
pub fn markup(
    code: &str,
    language: Option<&sourceview::Language>,
    scheme: Option<&sourceview::StyleScheme>,
) -> String {
    let code = code.trim_end();
    let Some(language) = language else {
        return format!("<tt>{}</tt>", glib::markup_escape_text(code));
    };
    let buffer = sourceview::Buffer::with_language(language);
    buffer.set_highlight_matching_brackets(false);
    buffer.set_style_scheme(scheme);
    buffer.set_text(code);
    let (mut start, end) = buffer.bounds();
    buffer.ensure_highlight(&start, &end);
    let mut markup = String::from("<tt>");
    while !start.is_end() {
        let mut next = start;
        next.forward_to_tag_toggle(None::<&gtk::TextTag>);
        // Tags come in ascending priority, so each span overrides those
        // around it, as each tag does those below it.
        let spans: Vec<String> = start.tags().iter().filter_map(span).collect();
        for span in &spans {
            markup.push_str(span);
        }
        markup.push_str(&glib::markup_escape_text(&buffer.text(&start, &next, true)));
        markup.push_str(&"</span>".repeat(spans.len()));
        start = next;
    }
    markup.push_str("</tt>");
    markup
}

/// The language a code block's info string names by its first word: as its
/// identifier, a common name for it or a file extension.
pub fn language(info: &str) -> Option<sourceview::Language> {
    let word = (info.split([' ', '\t', ',']).next())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let id = match word.as_str() {
        "py" | "python" => "python3",
        "javascript" => "js",
        "bash" | "shell" => "sh",
        "c++" => "cpp",
        "cs" | "csharp" => "c-sharp",
        word => word,
    };
    let manager = sourceview::LanguageManager::default();
    (manager.language(id)).or_else(|| manager.guess_language(Some(format!("code.{word}")), None))
}

/// A span giving text the look `tag` gives it, opened, if it gives any.
fn span(tag: &gtk::TextTag) -> Option<String> {
    let mut attributes = Vec::new();
    if tag.is_foreground_set()
        && let Some(color) = tag.foreground_rgba()
    {
        attributes.push(format!("foreground=\"{}\"", hex(color)));
    }
    if tag.is_background_set()
        && let Some(color) = tag.background_rgba()
    {
        attributes.push(format!("background=\"{}\"", hex(color)));
    }
    if tag.is_weight_set() {
        attributes.push(format!("weight=\"{}\"", tag.weight()));
    }
    if tag.is_style_set() {
        let style = match tag.style() {
            pango::Style::Italic => "italic",
            pango::Style::Oblique => "oblique",
            _ => "normal",
        };
        attributes.push(format!("style=\"{style}\""));
    }
    if tag.is_underline_set() {
        let underline = match tag.underline() {
            pango::Underline::None => "none",
            pango::Underline::Double | pango::Underline::DoubleLine => "double",
            pango::Underline::Low => "low",
            pango::Underline::Error | pango::Underline::ErrorLine => "error",
            _ => "single",
        };
        attributes.push(format!("underline=\"{underline}\""));
    }
    if tag.is_strikethrough_set() {
        attributes.push(format!("strikethrough=\"{}\"", tag.is_strikethrough()));
    }
    (!attributes.is_empty()).then(|| format!("<span {}>", attributes.join(" ")))
}

/// `color` as Pango markup takes one with its alpha: `#rrggbbaa`.
fn hex(color: gdk::RGBA) -> String {
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "clamped to a byte"
    )]
    let byte = |value: f32| (value.clamp(0.0, 1.0) * 255.0).round() as u8;
    format!(
        "#{:02x}{:02x}{:02x}{:02x}",
        byte(color.red()),
        byte(color.green()),
        byte(color.blue()),
        byte(color.alpha())
    )
}
