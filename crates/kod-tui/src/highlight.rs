//! Token-level colouring for chat: fenced code blocks and unified diffs.
//!
//! Fenced blocks (` ```rust `) highlight per token via syntect's bundled
//! grammars. Unified diffs (`write_file` / `patch_file` / `git_diff` rows,
//! `/diff` output, ` ```diff ` fences) colour `+` / `-` / `@@` rows and
//! highlight the code inside each row when the language is known.
//!
//! Every entry point is total: unknown languages or missing theme dumps
//! fall back to the flat `theme.code` style. A render path never returns
//! `Result`.

use crate::theme::Theme;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;
use std::sync::OnceLock;
use syntect::easy::HighlightLines;
use syntect::highlighting::{FontStyle, Style as SyntectStyle, Theme as SyntectTheme, ThemeSet};
use syntect::parsing::{SyntaxReference, SyntaxSet};

/// Background painted behind code and diff bodies.
pub const CODE_BG: Color = Color::Rgb(28, 30, 38);
const ADD_BG: Color = Color::Rgb(23, 44, 31);
const DEL_BG: Color = Color::Rgb(54, 26, 31);
const SYNTECT_THEME: &str = "base16-eighties.dark";
const DIFF_FENCES: [&str; 4] = ["diff", "patch", "udiff", "gitdiff"];

struct Highlighter {
    syntaxes: SyntaxSet,
    theme: Option<SyntectTheme>,
}

fn highlighter() -> &'static Highlighter {
    static HL: OnceLock<Highlighter> = OnceLock::new();
    HL.get_or_init(|| {
        let themes = ThemeSet::load_defaults();
        let theme = themes.themes.get(SYNTECT_THEME).cloned().or_else(|| {
            themes
                .themes
                .iter()
                .find(|(n, _)| n.contains("dark"))
                .map(|(_, t)| t.clone())
        });
        Highlighter {
            syntaxes: SyntaxSet::load_defaults_newlines(),
            theme,
        }
    })
}

fn syntax_for(token: &str) -> Option<&'static SyntaxReference> {
    let t = token.trim();
    if t.is_empty() {
        return None;
    }
    let s = &highlighter().syntaxes;
    s.find_syntax_by_token(&t.to_ascii_lowercase())
        .filter(|x| !x.name.eq_ignore_ascii_case("plain text"))
}
/// Styled rows for a fenced code block. `lang` is the fence info string;
/// only its first word is used (`rust,ignore` -> `rust`). Unknown or
/// absent languages keep the flat `theme.code` colour; a `diff` fence
/// renders through `diff_rows`. Rows are truncated to `width`, never
/// wrapped. Grammar state carries across rows (block comments stay one
/// colour).
pub fn code_rows(
    lines: &[&str],
    lang: Option<&str>,
    width: usize,
    theme: &Theme,
) -> Vec<Vec<Span<'static>>> {
    let width = width.max(1);
    let token = lang.map(fence_token).unwrap_or_default();
    if DIFF_FENCES.contains(&token.as_str()) {
        return diff_rows(lines, None, width, theme);
    }
    let hl = highlighter();
    match (syntax_for(&token), hl.theme.as_ref()) {
        (Some(sx), Some(st)) => highlight_rows(lines, sx, st, width),
        _ => lines.iter().map(|l| flat_row(l, width, theme)).collect(),
    }
}

/// Flat `theme.code` style on the code background.
pub fn code_style(theme: &Theme) -> Style {
    Style::default().fg(theme.code).bg(CODE_BG)
}

fn flat_row(line: &str, width: usize, theme: &Theme) -> Vec<Span<'static>> {
    truncate(
        vec![Span::styled(line.to_string(), code_style(theme))],
        width,
    )
}

fn highlight_rows(
    lines: &[&str],
    syntax: &SyntaxReference,
    syn_theme: &SyntectTheme,
    width: usize,
) -> Vec<Vec<Span<'static>>> {
    let syntaxes = &highlighter().syntaxes;
    let mut h = HighlightLines::new(syntax, syn_theme);
    lines
        .iter()
        .map(|line| {
            let mut spans = Vec::new();
            let term = format!("{line}\n");
            for (st, tx) in h.highlight_line(&term, syntaxes).unwrap_or_default() {
                push_span(&mut spans, tx, convert_style(st).bg(CODE_BG));
            }
            trim_newline(&mut spans);
            truncate(spans, width)
        })
        .collect()
}
/// A unified diff found inside a message body. Tool rows carry one as
/// `path:\n@@ ...`, `/diff` carries one after a prose line, the model
/// writes them in ` ```diff ` fences.
#[derive(Debug, Clone)]
pub struct DiffBody<'a> {
    pub lead: Vec<&'a str>,
    pub lines: Vec<&'a str>,
    pub syntax: Option<&'static SyntaxReference>,
}

impl<'a> DiffBody<'a> {
    /// Split `body` at the first diff header (`diff --git`, a hunk
    /// header with a range, or a `---`/`+++` pair). `None` when there is
    /// no diff. A bare `@@` is not enough: prose that *describes* a hunk
    /// must not flip the message into diff rendering.
    pub fn parse(body: &'a str) -> Option<DiffBody<'a>> {
        let all: Vec<&str> = body
            .lines()
            .map(|l| l.strip_suffix('\r').unwrap_or(l))
            .collect();
        let start = diff_start(&all)?;
        Some(DiffBody {
            lead: all[..start].to_vec(),
            lines: all[start..].to_vec(),
            syntax: diff_syntax(&all[..start], &all[start..]),
        })
    }
}

/// Styled rows for a unified diff. Hunk headers take the accent colour,
/// metadata (`diff --git`, `index`, `\ No newline`) is dim, `+`/`-` rows
/// are tinted green/red behind a bold marker; when `syntax` is known the
/// code inside each row is coloured per token (the tint carries the
/// `+`/`-` reading, since tokens no longer do). Rows are truncated, never
/// wrapped: a wrapped `+` row reads as two rows the patch lacks.
pub fn diff_rows(
    lines: &[&str],
    syntax: Option<&'static SyntaxReference>,
    width: usize,
    theme: &Theme,
) -> Vec<Vec<Span<'static>>> {
    let width = width.max(1);
    let hl = highlighter();
    let mut h = syntax
        .zip(hl.theme.as_ref())
        .map(|(sx, st)| HighlightLines::new(sx, st));
    lines
        .iter()
        .map(|l| diff_row(l, h.as_mut(), &hl.syntaxes, width, theme))
        .collect()
}

fn diff_row(
    line: &str,
    h: Option<&mut HighlightLines>,
    sx: &SyntaxSet,
    width: usize,
    theme: &Theme,
) -> Vec<Span<'static>> {
    if is_hunk(line) {
        let st = Style::default()
            .fg(theme.accent)
            .add_modifier(Modifier::BOLD);
        return truncate(vec![Span::styled(line.to_string(), st)], width);
    }
    let Some((m, content)) = split_marker(line) else {
        return truncate(
            vec![Span::styled(
                line.to_string(),
                Style::default().fg(theme.dim),
            )],
            width,
        );
    };
    let (ms, tint, cs) = match m {
        '+' => (
            Style::default().fg(theme.user).add_modifier(Modifier::BOLD),
            Some(ADD_BG),
            Style::default().fg(theme.user),
        ),
        '-' => (
            Style::default()
                .fg(theme.error)
                .add_modifier(Modifier::BOLD),
            Some(DEL_BG),
            Style::default().fg(theme.error),
        ),
        _ => (
            Style::default().fg(theme.dim),
            None,
            Style::default().fg(theme.foreground),
        ),
    };
    let mut spans = vec![Span::styled(m.to_string(), bg(ms, tint))];
    match h {
        Some(h) => {
            let extra = bg(Style::default(), tint);
            let term = format!("{content}\n");
            let mut cs_spans = Vec::new();
            for (st, tx) in h.highlight_line(&term, sx).unwrap_or_default() {
                push_span(&mut cs_spans, tx, convert_style(st).patch(extra));
            }
            trim_newline(&mut cs_spans);
            spans.extend(cs_spans);
        }
        None => spans.push(Span::styled(content.to_string(), bg(cs, tint))),
    }
    truncate(spans, width)
}

fn split_marker(line: &str) -> Option<(char, &str)> {
    if line.starts_with("+++") || line.starts_with("---") {
        return None;
    }
    match line.chars().next()? {
        m @ ('+' | '-' | ' ') => Some((m, &line[1..])),
        _ => None,
    }
}

fn is_hunk(line: &str) -> bool {
    line.starts_with("@@ -") || line.starts_with("@@@")
}

fn diff_start(lines: &[&str]) -> Option<usize> {
    for (i, l) in lines.iter().enumerate() {
        if l.starts_with("diff --git ") || is_hunk(l) {
            return Some(i);
        }
        if l.starts_with("--- ") && lines.get(i + 1).is_some_and(|n| n.starts_with("+++ ")) {
            return Some(i);
        }
    }
    None
}

fn diff_syntax(lead: &[&str], lines: &[&str]) -> Option<&'static SyntaxReference> {
    for l in lines {
        if let Some(rest) = l.strip_prefix("+++ ")
            && let Some(sx) = syntax_from_path(rest)
        {
            return Some(sx);
        }
    }
    lead.iter().rev().find_map(|l| syntax_from_path(l))
}

fn syntax_from_path(path: &str) -> Option<&'static SyntaxReference> {
    let p = path.trim();
    let p = p.split(' ').next().unwrap_or(p);
    let p = p.split(':').next().unwrap_or(p);
    let p = p
        .strip_prefix("a/")
        .or_else(|| p.strip_prefix("b/"))
        .unwrap_or(p);
    let name = p.rsplit('/').next().unwrap_or(p);
    let (_, ext) = name.rsplit_once('.')?;
    if ext.is_empty()
        || !ext
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '_'))
    {
        return None;
    }
    syntax_for(&ext.to_ascii_lowercase())
}

fn fence_token(info: &str) -> String {
    info.split_whitespace()
        .next()
        .unwrap_or("")
        .split(',')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase()
}
fn convert_style(st: SyntectStyle) -> Style {
    let fg = st.foreground;
    let mut out = Style::default().fg(Color::Rgb(fg.r, fg.g, fg.b));
    if st.font_style.contains(FontStyle::BOLD) {
        out = out.add_modifier(Modifier::BOLD);
    }
    if st.font_style.contains(FontStyle::ITALIC) {
        out = out.add_modifier(Modifier::ITALIC);
    }
    if st.font_style.contains(FontStyle::UNDERLINE) {
        out = out.add_modifier(Modifier::UNDERLINED);
    }
    out
}

fn push_span(spans: &mut Vec<Span<'static>>, text: &str, style: Style) {
    if text.is_empty() {
        return;
    }
    if let Some(last) = spans.last_mut()
        && last.style == style
    {
        let mut merged = last.content.clone().into_owned();
        merged.push_str(text);
        *last = Span::styled(merged, style);
        return;
    }
    spans.push(Span::styled(text.to_string(), style));
}

fn trim_newline(spans: &mut Vec<Span<'static>>) {
    if let Some(last) = spans.last_mut()
        && last.content.ends_with('\n')
    {
        let st = last.style;
        let t = last.content.trim_end_matches('\n').to_string();
        *last = Span::styled(t, st);
    }
}

fn bg(style: Style, tint: Option<Color>) -> Style {
    match tint {
        Some(b) => style.bg(b),
        None => style,
    }
}

/// Cut `spans` to `width` columns (ratatui's own widths, so the measuring
/// `Paragraph` never re-wraps), appending `…` when cut.
fn truncate(mut spans: Vec<Span<'static>>, width: usize) -> Vec<Span<'static>> {
    let total: usize = spans.iter().map(Span::width).sum();
    if total <= width {
        spans.retain(|s| !s.content.is_empty());
        return spans;
    }
    let budget = width.saturating_sub(1);
    let mut out: Vec<Span<'static>> = Vec::new();
    let mut used = 0usize;
    let mut es = Style::default();
    for span in spans {
        es = span.style;
        if used >= budget {
            break;
        }
        let mut kept = String::new();
        for ch in span.content.chars() {
            let cw = Span::raw(ch.to_string()).width().max(1);
            if used + cw > budget {
                break;
            }
            kept.push(ch);
            used += cw;
        }
        if !kept.is_empty() {
            out.push(Span::styled(kept, span.style));
        }
        if used >= budget {
            break;
        }
    }
    out.push(Span::styled("…", es));
    out
}
#[cfg(test)]
mod tests {
    use super::*;

    fn theme() -> Theme {
        Theme::dark()
    }
    fn txt(row: &[Span<'static>]) -> String {
        row.iter().map(|s| s.content.as_ref()).collect()
    }
    fn distinct(row: &[Span<'static>]) -> usize {
        let mut seen: Vec<Option<Color>> = Vec::new();
        for f in row.iter().map(|s| s.style.fg) {
            if !seen.contains(&f) {
                seen.push(f);
            }
        }
        seen.len()
    }

    #[test]
    fn bundled_theme_loads() {
        assert!(highlighter().theme.is_some());
    }

    #[test]
    fn common_languages_resolve() {
        for (tok, name) in [
            ("rs", "Rust"),
            ("python", "Python"),
            ("js", "JavaScript"),
            ("json", "JSON"),
            ("sh", "Bourne Again Shell (bash)"),
            ("go", "Go"),
            ("c", "C"),
            ("md", "Markdown"),
            ("yaml", "YAML"),
        ] {
            let sx = syntax_for(tok).unwrap_or_else(|| panic!("no grammar for {tok}"));
            assert_eq!(sx.name, name, "{tok}");
        }
        assert!(syntax_for("ts").is_none(), "no bundled TypeScript");
        assert!(syntax_for("nope").is_none());
        assert!(syntax_for("").is_none());
    }

    #[test]
    fn rust_gets_token_colours() {
        let src = "fn main() { let x = \"s\"; // c\n}";
        let rows = code_rows(&[src], Some("rust"), 80, &theme());
        assert_eq!(txt(&rows[0]), src);
        assert!(distinct(&rows[0]) >= 3);
        assert!(rows[0].iter().all(|s| s.style.bg == Some(CODE_BG)));
    }

    #[test]
    fn unknown_language_stays_flat() {
        for lang in [None, Some("nope")] {
            let rows = code_rows(&["plain"], lang, 80, &theme());
            assert_eq!(rows[0].len(), 1);
            assert_eq!(rows[0][0].style.fg, Some(theme().code));
            assert_eq!(rows[0][0].style.bg, Some(CODE_BG));
        }
    }

    #[test]
    fn code_truncates_to_width() {
        let long = "x".repeat(200);
        let rows = code_rows(&[&long], Some("rust"), 20, &theme());
        let w: usize = rows[0].iter().map(Span::width).sum();
        assert!(w <= 20);
        assert!(txt(&rows[0]).ends_with('…'));
    }

    #[test]
    fn comment_colour_carries_across_rows() {
        let rows = code_rows(
            &["/* start", "still comment */"],
            Some("rust"),
            80,
            &theme(),
        );
        assert_eq!(rows[0][0].style.fg, rows[1][0].style.fg);
    }

    #[test]
    fn tool_row_body_parses() {
        let d =
            DiffBody::parse("src/main.rs:\n@@ -1 +1,2 @@\n let a = 1;\n+let b = 2;").expect("diff");
        assert_eq!(d.lead, vec!["src/main.rs:"]);
        assert_eq!(d.lines[0], "@@ -1 +1,2 @@");
        assert!(d.syntax.is_some_and(|s| s.name.contains("Rust")));
    }

    #[test]
    fn git_header_parses() {
        let d = DiffBody::parse("diff --git a/a.rs b/a.rs\nindex 1..2 100644\n--- a/a.rs\n+++ b/a.rs\n@@ -1 +1 @@\n-a\n+b").expect("diff");
        assert!(d.lead.is_empty());
        assert!(d.syntax.is_some());
    }

    #[test]
    fn prose_is_not_a_diff() {
        for b in [
            "hello",
            "the token @@ is used",
            "1. list\n- bullet",
            "---",
            "--- rule, no +++ line",
        ] {
            assert!(DiffBody::parse(b).is_none(), "{b:?}");
        }
    }

    #[test]
    fn prose_lead_does_not_name_a_language() {
        let d = DiffBody::parse("Most recent change (cp-1 · /tmp/x/main.rs)\n\n--- a/main.rs\n+++ b/main.rs\n@@ -1 +1 @@\n-a\n+b").expect("diff");
        assert_eq!(d.lead.len(), 2);
        assert!(d.syntax.is_some());
    }

    #[test]
    fn add_remove_hunk_colours() {
        let t = theme();
        let rows = diff_rows(
            &["-gone", "+added", "@@ -1 +1 @@", "diff --git a/x b/x"],
            None,
            40,
            &t,
        );
        assert_eq!(rows[0][0].style.fg, Some(t.error));
        assert_eq!(rows[1][0].style.fg, Some(t.user));
        assert!(rows[1][0].style.add_modifier.contains(Modifier::BOLD));
        assert_ne!(rows[0][0].style.bg, rows[1][0].style.bg);
        assert_eq!(rows[2][0].style.fg, Some(t.accent));
        assert_eq!(rows[3][0].style.fg, Some(t.dim));
        assert_eq!(txt(&rows[1]), "+added");
    }

    #[test]
    fn context_row_keeps_terminal_bg() {
        let t = theme();
        let rows = diff_rows(&[" context"], None, 40, &t);
        assert_eq!(rows[0][1].style.bg, None);
        assert_eq!(rows[0][1].style.fg, Some(t.foreground));
        assert_eq!(txt(&rows[0]), " context");
    }

    #[test]
    fn diff_code_is_highlighted_and_text_survives() {
        let t = theme();
        let d = DiffBody::parse("main.rs:\n@@ -1,1 +1,2 @@\n+fn main() { let x = \"s\"; // c\n+}")
            .expect("diff");
        let rows = diff_rows(&d.lines, d.syntax, 80, &t);
        assert_eq!(rows.len(), 3);
        assert_eq!(txt(&rows[1]), "+fn main() { let x = \"s\"; // c");
        assert!(distinct(&rows[1]) >= 3);
    }

    #[test]
    fn diff_truncates() {
        let t = theme();
        let long = format!("+{}", "y".repeat(120));
        let rows = diff_rows(&[&long], None, 20, &t);
        let w: usize = rows[0].iter().map(Span::width).sum();
        assert!(w <= 20);
        assert!(txt(&rows[0]).ends_with('…'));
    }

    #[test]
    fn diff_fence_renders_as_diff() {
        let t = theme();
        let rows = code_rows(&["@@ -1 +1 @@", "-old", "+new"], Some("diff"), 40, &t);
        assert_eq!(rows[0][0].style.fg, Some(t.accent));
        assert_eq!(rows[2][0].style.fg, Some(t.user));
    }
}
