//! Markdown, as servers describe code in, as Pango markup for labels: in the
//! parts its rules separate, with headings and strong text in bold, code in
//! a monospace font, and links to follow.

use std::mem;

use gtk::glib;
use pulldown_cmark::{CodeBlockKind, Event, Parser, Tag, TagEnd};

/// `markdown` as Pango markup, in the parts between its rules. `code` gives
/// each code block's markup from its fence's info string, empty if it has
/// none, and its text.
pub fn markup(markdown: &str, code: &impl Fn(&str, &str) -> String) -> Vec<String> {
    let mut writer = Writer {
        code,
        block: None,
        parts: Vec::new(),
        part: String::new(),
        gap: "",
        lists: Vec::new(),
        bulleted: false,
    };
    for event in Parser::new(markdown) {
        writer.event(event);
    }
    writer.rule();
    writer.parts
}

struct Writer<'a, F> {
    code: &'a F,
    /// The code block being read, with its info string.
    block: Option<(String, String)>,
    parts: Vec<String>,
    part: String,
    /// What goes before the next text, as blocks begin: a blank line between
    /// blocks, or a line break between items.
    gap: &'static str,
    /// The lists the text is in, innermost last, with an ordered list's next
    /// number.
    lists: Vec<Option<u64>>,
    /// Whether an item's bullet was just written, which its first block
    /// follows on the same line.
    bulleted: bool,
}

impl<F: Fn(&str, &str) -> String> Writer<'_, F> {
    fn event(&mut self, event: Event) {
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(text) => match &mut self.block {
                Some((_, code)) => code.push_str(&text),
                None => self.text(&glib::markup_escape_text(&text)),
            },
            // HTML's text, without its tags. An HTML block comes a line at
            // a time, while inline HTML is a tag alone, which is left out.
            Event::Html(line) => {
                let line = without_tags(&line);
                if !line.trim().is_empty() {
                    self.text(&glib::markup_escape_text(line.trim_end()));
                    self.gap = "\n";
                }
            }
            Event::Code(text) => {
                self.text(&format!("<tt>{}</tt>", glib::markup_escape_text(&text)));
            }
            Event::SoftBreak => self.text(" "),
            Event::HardBreak => self.text("\n"),
            Event::Rule => self.rule(),
            _ => {}
        }
    }

    fn start(&mut self, tag: Tag) {
        if matches!(
            tag,
            Tag::Paragraph
                | Tag::Heading { .. }
                | Tag::CodeBlock(_)
                | Tag::HtmlBlock
                | Tag::List(_)
        ) && !self.bulleted
        {
            self.gap = if self.lists.is_empty() { "\n\n" } else { "\n" };
        }
        match tag {
            Tag::Heading { .. } | Tag::Strong => self.text("<b>"),
            Tag::Emphasis => self.text("<i>"),
            Tag::Strikethrough => self.text("<s>"),
            Tag::CodeBlock(kind) => {
                let info = match kind {
                    CodeBlockKind::Fenced(info) => info.into_string(),
                    CodeBlockKind::Indented => String::new(),
                };
                self.block = Some((info, String::new()));
            }
            Tag::Link { dest_url, .. } => {
                self.text(&format!(
                    "<a href=\"{}\">",
                    glib::markup_escape_text(&dest_url)
                ));
            }
            Tag::List(first) => self.lists.push(first),
            Tag::Item => {
                if self.gap.is_empty() {
                    self.gap = "\n";
                }
                let indent = "  ".repeat(self.lists.len().saturating_sub(1));
                let bullet = match self.lists.last_mut() {
                    Some(Some(number)) => {
                        *number += 1;
                        format!("{}. ", *number - 1)
                    }
                    _ => "• ".to_owned(),
                };
                self.text(&format!("{indent}{bullet}"));
                self.bulleted = true;
            }
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Heading(_) | TagEnd::Strong => self.part.push_str("</b>"),
            TagEnd::Emphasis => self.part.push_str("</i>"),
            TagEnd::Strikethrough => self.part.push_str("</s>"),
            TagEnd::Link => self.part.push_str("</a>"),
            TagEnd::CodeBlock => {
                if let Some((info, code)) = self.block.take() {
                    self.text(&(self.code)(&info, &code));
                }
            }
            TagEnd::List(_) => {
                self.lists.pop();
            }
            _ => {}
        }
    }

    /// Writes `markup`, after any gap that the blocks before call for.
    fn text(&mut self, markup: &str) {
        if !self.part.is_empty() {
            self.part.push_str(self.gap);
        }
        self.gap = "";
        self.bulleted = false;
        self.part.push_str(markup);
    }

    /// Ends the part written so far, if anything was.
    fn rule(&mut self) {
        if !self.part.is_empty() {
            self.parts.push(mem::take(&mut self.part));
        }
        self.gap = "";
    }
}

/// `html` without its tags.
fn without_tags(html: &str) -> String {
    let mut text = String::new();
    let mut rest = html;
    while let Some(start) = rest.find('<') {
        text.push_str(&rest[..start]);
        rest = rest[start..]
            .find('>')
            .map_or("", |end| &rest[start + end + 1..]);
    }
    text.push_str(rest);
    text
}

#[cfg(test)]
mod tests {
    use gtk::glib;

    /// `markdown` as markup, with code blocks marked with their info strings.
    fn markup(markdown: &str) -> Vec<String> {
        super::markup(markdown, &|info, code| {
            format!(
                "<tt>[{info}]{}</tt>",
                glib::markup_escape_text(code.trim_end())
            )
        })
    }

    #[test]
    fn blocks_are_spaced_and_styled() {
        assert_eq!(
            markup("# Title\nSome *very* **bold** text,\nwrapped.\n\n```rust\nlet a = 1 < 2;\n```"),
            [
                "<b>Title</b>\n\nSome <i>very</i> <b>bold</b> text, wrapped.\n\n<tt>[rust]let a = 1 &lt; 2;</tt>"
            ]
        );
    }

    #[test]
    fn rules_separate_parts() {
        assert_eq!(
            markup("```rust,ignore\nfn f()\n```\n\n---\n\nDoes `f` & more.\n\n    indented"),
            [
                "<tt>[rust,ignore]fn f()</tt>",
                "Does <tt>f</tt> &amp; more.\n\n<tt>[]indented</tt>"
            ]
        );
        assert_eq!(markup("---"), Vec::<String>::new());
    }

    #[test]
    fn lists_have_bullets_and_numbers() {
        assert_eq!(
            markup("Steps:\n\n1. one\n2. two\n   - deep\n\n- loose\n\n  item"),
            ["Steps:\n\n1. one\n2. two\n  • deep\n\n• loose\nitem"]
        );
    }

    #[test]
    fn html_is_left_out_but_for_its_text() {
        assert_eq!(
            markup(
                "Use the <a href=\"#\" onclick=\"f();\">search button</a>.\n\n<div class=\"warning\">\n\nHeed <b>this</b>.\n</div>\n<p>\nOne\nTwo\n</p>\n\nDone."
            ),
            ["Use the search button.\n\nHeed this.\n\nOne\nTwo\n\nDone."]
        );
    }

    #[test]
    fn links_can_be_followed() {
        assert_eq!(
            markup("See [`LazyCell`](https://doc.rust-lang.org/a?b=1&c=\"d\")."),
            [
                "See <a href=\"https://doc.rust-lang.org/a?b=1&amp;c=&quot;d&quot;\"><tt>LazyCell</tt></a>."
            ]
        );
    }
}
