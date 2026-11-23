//! Syntax highlighting for code blocks: syntect with the languages and themes
//! of `two-face` (bat's set of about 200: Rust, Python, JavaScript/TypeScript,
//! C/C++/C#, Java, Kotlin, Go, shell, SQL, JSON, YAML, TOML, Nix, Lua,
//! HTML/CSS, Dockerfile, Zig, and many more), turned into Pango markup.

use std::sync::LazyLock;

use gtk::glib;
use syntect::easy::HighlightLines;
use syntect::highlighting::FontStyle;
use syntect::parsing::{SyntaxReference, SyntaxSet};
use syntect::util::LinesWithEndings;
use two_face::theme::{EmbeddedLazyThemeSet, EmbeddedThemeName};

static SYNTAXES: LazyLock<SyntaxSet> = LazyLock::new(two_face::syntax::extra_newlines);
static THEMES: LazyLock<EmbeddedLazyThemeSet> = LazyLock::new(two_face::theme::extra);

/// The longest code block that is highlighted; longer ones show plain, so a
/// huge paste cannot stall the window.
const MAX_HIGHLIGHT: usize = 20_000;

/// Loads the languages ahead of the first code block (off the main thread).
pub fn warm() {
    LazyLock::force(&SYNTAXES);
}

/// The syntax for a code block's language tag: a name (`rust`), a file
/// extension (`rs`), or a common alias Discord users type.
pub(crate) fn syntax_for(language: &str) -> Option<&'static SyntaxReference> {
    let language = language.trim().to_ascii_lowercase();
    let token = match language.as_str() {
        "c++" => "cpp",
        "c#" | "csharp" => "cs",
        "golang" => "go",
        "shell" | "console" | "zsh" | "fish" => "bash",
        "js" | "node" => "javascript",
        "ts" => "typescript",
        "py" | "python3" => "python",
        "rb" => "ruby",
        "kt" => "kotlin",
        "yml" => "yaml",
        "md" => "markdown",
        "objc" | "objective-c" => "m",
        "docker" => "dockerfile",
        "asm" | "nasm" | "x86" => "x86_64 Assembly",
        "vue" => "Vue Component",
        "tf" | "hcl" => "terraform",
        "proto" | "protobuf" => "Protocol Buffer",
        "jsonc" | "json5" => "json",
        other => other,
    };
    let syntaxes = &*SYNTAXES;
    syntaxes
        .find_syntax_by_token(token)
        .or_else(|| syntaxes.find_syntax_by_name(&language))
        .or_else(|| {
            syntaxes
                .syntaxes()
                .iter()
                .find(|syntax| syntax.name.eq_ignore_ascii_case(&language))
        })
}

/// `code` as Pango markup, highlighted for `language` in the dark or light
/// theme; `None` when the language is unknown (or the code is huge).
pub(crate) fn highlight(code: &str, language: &str, dark: bool) -> Option<String> {
    if code.len() > MAX_HIGHLIGHT {
        return None;
    }
    let syntax = syntax_for(language)?;
    let theme = THEMES.get(if dark {
        EmbeddedThemeName::OneHalfDark
    } else {
        EmbeddedThemeName::OneHalfLight
    });
    let mut lines = HighlightLines::new(syntax, theme);
    let mut markup = String::with_capacity(code.len() * 2);
    for line in LinesWithEndings::from(code) {
        let regions = lines.highlight_line(line, &SYNTAXES).ok()?;
        for (style, text) in regions {
            let color = style.foreground;
            markup.push_str(&format!(
                "<span foreground=\"#{:02x}{:02x}{:02x}\"",
                color.r, color.g, color.b
            ));
            if style.font_style.contains(FontStyle::BOLD) {
                markup.push_str(" weight=\"bold\"");
            }
            if style.font_style.contains(FontStyle::ITALIC) {
                markup.push_str(" style=\"italic\"");
            }
            markup.push('>');
            markup.push_str(&glib::markup_escape_text(text));
            markup.push_str("</span>");
        }
    }
    // The final line break would show as a blank line.
    while markup.ends_with("\n</span>") {
        let cut = markup.len() - "\n</span>".len();
        markup.replace_range(cut..cut + 1, "");
    }
    Some(markup)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn common_languages_and_aliases_are_known() {
        for language in [
            "rust", "rs", "python", "py", "javascript", "js", "typescript", "ts", "tsx", "c", "cpp",
            "c++", "cs", "csharp", "java", "kotlin", "kt", "go", "golang", "bash", "sh", "shell",
            "zsh", "sql", "json", "yaml", "yml", "toml", "nix", "lua", "html", "css", "dockerfile",
            "ruby", "php", "swift", "haskell", "markdown", "diff", "xml", "asm", "vue", "proto",
            "zig", "dart", "scala", "elixir", "graphql", "terraform", "makefile", "cmake", "ini",
        ] {
            assert!(syntax_for(language).is_some(), "no syntax for {language}");
        }
        assert!(syntax_for("not-a-language").is_none());
    }

    #[test]
    fn highlighting_colors_the_code_and_keeps_its_text() {
        let markup = highlight("fn main() {\n    let x = 1;\n}\n", "rust", true).unwrap();
        assert!(markup.contains("<span foreground=\"#"));
        let (_, text, _) = gtk::pango::parse_markup(&markup, '\0').unwrap();
        assert_eq!(text, "fn main() {\n    let x = 1;\n}");
        // Markup characters in the code are escaped.
        let markup = highlight("if a < b && c > d {}", "rust", false).unwrap();
        assert!(gtk::pango::parse_markup(&markup, '\0').is_ok());
        assert!(highlight("x", "unknown-language", true).is_none());
    }
}
