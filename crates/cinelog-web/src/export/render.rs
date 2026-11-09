//! Rendered terminal rows of a range, as plain text or as escaped, inline-styled HTML
//! (plan §7.6). Rows are taken from an avt terminal as they scroll off the screen, as the
//! alternate screen is left, before a full reset, and from the screen at the end; soft
//! wrapped rows are joined into one line.

use std::fmt::Write as _;

use avt::observer::Observation;
use avt::parser::Function;
use avt::{Color, Line, Observer, Operation, Pen, ScreenKind, Terminal};

/// One run of cells with the same pen.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Run {
    pub pen: Pen,
    pub text: String,
}

/// One physical row, owned.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Row {
    pub runs: Vec<Run>,
    pub wrapped: bool,
}

impl Row {
    fn of(line: &Line) -> Self {
        let runs = line
            .chunks(|a, b| a.pen() != b.pen())
            .filter_map(|cells| {
                let pen = *cells.first()?.pen();
                Some(Run {
                    pen,
                    text: cells.iter().map(avt::Cell::char).collect(),
                })
            })
            .collect();
        Self {
            runs,
            wrapped: line.is_wrapped(),
        }
    }

    fn is_blank(&self) -> bool {
        self.runs.iter().all(visible_blank)
    }
}

/// A run that shows nothing: spaces without a background.
fn visible_blank(r: &Run) -> bool {
    r.pen.background().is_none() && !r.pen.is_inverse() && r.text.chars().all(|c| c == ' ')
}

/// Collects finished rows from the terminal's callbacks, in time order.
#[derive(Default)]
struct Collector {
    out: Vec<Row>,
    /// Top rows of the primary screen already in `out` (written out when the alternate
    /// screen was entered); they are not repeated when they scroll off.
    flushed: usize,
    kind_before: Option<ScreenKind>,
    /// The alternate screen as it was before the current operation.
    alt_before: Option<Vec<Row>>,
    last_alt: Option<Vec<Row>>,
}

/// Visible rows without trailing blank ones.
fn rows_of<'a>(lines: impl Iterator<Item = &'a Line>) -> Vec<Row> {
    let mut rows: Vec<Row> = lines.map(Row::of).collect();
    while rows.last().is_some_and(Row::is_blank) {
        rows.pop();
    }
    rows
}

impl Observer for Collector {
    type Output = ();

    fn before(&mut self, ctx: &mut Observation<'_>, op: &Operation<'_>) {
        let vt = ctx.terminal();
        let kind = vt.active_screen_kind();
        self.kind_before = Some(kind);
        match op {
            // A reset discards the screen without eviction callbacks.
            Operation::Function(Function::Ris) => {
                if kind == ScreenKind::Alternate {
                    self.push_alt(rows_of(vt.screen().visible_lines()));
                }
                self.flush_primary(vt);
                self.flushed = 0;
            }
            Operation::Function(Function::Decrst(_)) if kind == ScreenKind::Alternate => {
                self.alt_before = Some(rows_of(vt.screen().visible_lines()));
            }
            _ => {}
        }
    }

    fn after(&mut self, ctx: &mut Observation<'_>, _op: &Operation<'_>) {
        let vt = ctx.terminal();
        let now = vt.active_screen_kind();
        match (self.kind_before.take(), now) {
            (Some(ScreenKind::Primary), ScreenKind::Alternate) => self.flush_primary(vt),
            (Some(ScreenKind::Alternate), ScreenKind::Primary) => {
                if let Some(rows) = self.alt_before.take() {
                    self.push_alt(rows);
                }
            }
            _ => {}
        }
        self.alt_before = None;
    }

    fn evicted(&mut self, _ctx: &mut Observation<'_>, line: &Line) {
        if self.flushed > 0 {
            self.flushed -= 1;
        } else {
            self.out.push(Row::of(line));
        }
    }

    fn end(&mut self, _ctx: &mut Observation<'_>) {}
}

impl Collector {
    /// Writes out the primary screen's rows that are not out yet.
    fn flush_primary(&mut self, vt: &Terminal) {
        let rows = rows_of(vt.primary_screen().visible_lines());
        let n = rows.len();
        if n > self.flushed {
            self.out.extend(rows.into_iter().skip(self.flushed));
            self.flushed = n;
        }
    }

    /// Alternate screens are kept once per distinct picture.
    fn push_alt(&mut self, rows: Vec<Row>) {
        if rows.is_empty() || self.last_alt.as_ref() == Some(&rows) {
            return;
        }
        self.last_alt = Some(rows.clone());
        self.out.extend(rows);
    }
}

/// Renders output into rows.
pub(crate) struct Renderer {
    vt: Terminal,
    obs: Collector,
}

impl Renderer {
    pub fn new(size: (u16, u16)) -> Self {
        Self {
            vt: crate::play::plan::terminal(size, ""),
            obs: Collector::default(),
        }
    }

    pub fn output(&mut self, data: &str) {
        self.vt.feed_with(data, &mut self.obs);
    }

    pub fn resize(&mut self, cols: u16, rows: u16) {
        let Some((cols, rows)) = cinelog_cast::sane_size(cols, rows) else {
            return;
        };
        self.vt.resize_with(usize::from(cols), usize::from(rows), &mut self.obs);
    }

    /// Rows finished so far.
    pub fn take(&mut self) -> Vec<Row> {
        std::mem::take(&mut self.obs.out)
    }

    /// Every remaining row, including the screen.
    pub fn finish(mut self) -> Vec<Row> {
        self.obs.flush_primary(&self.vt);
        if self.vt.active_screen_kind() == ScreenKind::Alternate {
            let rows = rows_of(self.vt.screen().visible_lines());
            self.obs.push_alt(rows);
        }
        self.obs.out
    }
}

/// Joins soft-wrapped rows into logical lines and writes them as text or HTML.
pub(crate) struct LineWriter {
    html: bool,
    pending: Vec<Run>,
}

impl LineWriter {
    pub fn new(html: bool) -> Self {
        Self {
            html,
            pending: Vec::new(),
        }
    }

    pub fn rows(&mut self, rows: Vec<Row>, out: &mut Vec<u8>) {
        for row in rows {
            self.pending.extend(row.runs);
            if !row.wrapped {
                self.line(out);
            }
        }
    }

    pub fn finish(&mut self, out: &mut Vec<u8>) {
        if !self.pending.is_empty() {
            self.line(out);
        }
    }

    fn line(&mut self, out: &mut Vec<u8>) {
        let mut runs = std::mem::take(&mut self.pending);
        while runs.last().is_some_and(visible_blank) {
            runs.pop();
        }
        if let Some(last) = runs.last_mut()
            && last.pen.background().is_none()
            && !last.pen.is_inverse()
        {
            let keep = last.text.trim_end_matches(' ').len();
            last.text.truncate(keep);
        }
        let mut s = String::new();
        for r in &runs {
            if self.html {
                html_run(&mut s, r);
            } else {
                s.push_str(&r.text);
            }
        }
        s.push('\n');
        out.extend_from_slice(s.as_bytes());
    }
}

// ------------------------------------------------------------------ HTML

pub(crate) const DEFAULT_FG: &str = "#d0d0d0";
pub(crate) const DEFAULT_BG: &str = "#1c1c1c";

/// The 16 base colours (xterm defaults).
const BASE16: [(u8, u8, u8); 16] = [
    (0x00, 0x00, 0x00),
    (0xcd, 0x00, 0x00),
    (0x00, 0xcd, 0x00),
    (0xcd, 0xcd, 0x00),
    (0x00, 0x00, 0xee),
    (0xcd, 0x00, 0xcd),
    (0x00, 0xcd, 0xcd),
    (0xe5, 0xe5, 0xe5),
    (0x7f, 0x7f, 0x7f),
    (0xff, 0x00, 0x00),
    (0x00, 0xff, 0x00),
    (0xff, 0xff, 0x00),
    (0x5c, 0x5c, 0xff),
    (0xff, 0x00, 0xff),
    (0x00, 0xff, 0xff),
    (0xff, 0xff, 0xff),
];

/// `#rrggbb` of an SGR colour (256-colour palette or true colour).
pub(crate) fn css_color(c: Color) -> String {
    let (r, g, b) = match c {
        Color::RGB(rgb) => (rgb.r, rgb.g, rgb.b),
        Color::Indexed(i) if i < 16 => BASE16[usize::from(i)],
        Color::Indexed(i) if i < 232 => {
            let i = i - 16;
            let level = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
            (level(i / 36), level((i / 6) % 6), level(i % 6))
        }
        Color::Indexed(i) => {
            let v = 8 + (i - 232) * 10;
            (v, v, v)
        }
    };
    format!("#{r:02x}{g:02x}{b:02x}")
}

/// The inline style of a pen: colours, bold, faint, italic, underline, strike-through.
pub(crate) fn css_style(pen: &Pen) -> String {
    let (mut fg, mut bg) = (pen.foreground().map(css_color), pen.background().map(css_color));
    if pen.is_inverse() {
        let f = fg.take().unwrap_or_else(|| DEFAULT_FG.to_owned());
        let b = bg.take().unwrap_or_else(|| DEFAULT_BG.to_owned());
        fg = Some(b);
        bg = Some(f);
    }
    let mut s = String::new();
    if let Some(c) = fg {
        let _ = write!(s, "color:{c};");
    }
    if let Some(c) = bg {
        let _ = write!(s, "background-color:{c};");
    }
    if pen.is_bold() {
        s.push_str("font-weight:bold;");
    }
    if pen.is_faint() {
        s.push_str("opacity:0.7;");
    }
    if pen.is_italic() {
        s.push_str("font-style:italic;");
    }
    match (pen.is_underline(), pen.is_strikethrough()) {
        (true, true) => s.push_str("text-decoration:underline line-through;"),
        (true, false) => s.push_str("text-decoration:underline;"),
        (false, true) => s.push_str("text-decoration:line-through;"),
        (false, false) => {}
    }
    s
}

/// Appends `text` with every markup-relevant character escaped; control characters
/// become spaces.
pub(crate) fn escape_html(out: &mut String, text: &str) {
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c if c.is_control() => out.push(' '),
            c => out.push(c),
        }
    }
}

fn html_run(out: &mut String, r: &Run) {
    let style = css_style(&r.pen);
    if style.is_empty() {
        escape_html(out, &r.text);
    } else {
        // The style only ever contains the fixed property names and `#rrggbb` values.
        let _ = write!(out, "<span style=\"{style}\">");
        escape_html(out, &r.text);
        out.push_str("</span>");
    }
}

/// Start of the HTML document, up to the opening `<pre>`.
pub(crate) fn html_head(title: &str) -> String {
    let mut t = String::new();
    escape_html(&mut t, title);
    format!(
        "<!DOCTYPE html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n\
         <meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'none'; style-src 'unsafe-inline'\">\n\
         <meta name=\"referrer\" content=\"no-referrer\">\n<title>{t}</title>\n</head>\n\
         <body style=\"margin:0;background:{DEFAULT_BG};\">\n\
         <pre style=\"margin:0;padding:1em;color:{DEFAULT_FG};background:{DEFAULT_BG};\
         font-family:ui-monospace,monospace;font-size:13px;line-height:1.3;white-space:pre;\">"
    )
}

pub(crate) const HTML_TAIL: &str = "</pre>\n</body>\n</html>\n";

#[cfg(test)]
mod tests {
    use super::*;

    fn render(size: (u16, u16), data: &[&str], html: bool) -> String {
        let mut r = Renderer::new(size);
        let mut w = LineWriter::new(html);
        let mut out = Vec::new();
        for d in data {
            r.output(d);
            let rows = r.take();
            w.rows(rows, &mut out);
        }
        w.rows(r.finish(), &mut out);
        w.finish(&mut out);
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn scrolled_rows_and_the_final_screen() {
        let text = render((10, 3), &["one\r\ntwo\r\nthree\r\nfour\r\n"], false);
        assert_eq!(text, "one\ntwo\nthree\nfour\n");
    }

    #[test]
    fn soft_wraps_are_joined() {
        let text = render((5, 3), &["abcdefghij\r\nx\r\n"], false);
        assert_eq!(text, "abcdefghij\nx\n");
    }

    #[test]
    fn alternate_screen_is_kept_when_left() {
        let text = render(
            (12, 3),
            &[
                "before\r\n",
                "\x1b[?1049h\x1b[Hvim screen",
                "\x1b[?25l\x1b[?25h",
                "\x1b[?1049lafter\r\n",
            ],
            false,
        );
        assert_eq!(text, "before\nvim screen\nafter\n");
    }

    #[test]
    fn html_escapes_and_styles() {
        let html = render(
            (40, 3),
            &["<script>x</script> \x1b[1;31mred\x1b[0m \x1b[7minv\x1b[0m"],
            true,
        );
        assert!(html.contains("&lt;script&gt;x&lt;/script&gt;"), "{html}");
        assert!(!html.contains("<script"));
        assert!(
            html.contains("<span style=\"color:#cd0000;font-weight:bold;\">red</span>"),
            "{html}"
        );
        assert!(html.contains(&format!("color:{DEFAULT_BG};background-color:{DEFAULT_FG};")));
    }

    #[test]
    fn colours() {
        assert_eq!(css_color(Color::Indexed(1)), "#cd0000");
        assert_eq!(css_color(Color::Indexed(16)), "#000000");
        assert_eq!(css_color(Color::Indexed(231)), "#ffffff");
        assert_eq!(css_color(Color::Indexed(232)), "#080808");
        assert_eq!(css_color(Color::rgb(1, 2, 255)), "#0102ff");
        let mut s = String::new();
        escape_html(&mut s, "a&\"'<\u{7}");
        assert_eq!(s, "a&amp;&quot;&#39;&lt; ");
        assert!(html_head("x<y").contains("<title>x&lt;y</title>"));
    }
}
