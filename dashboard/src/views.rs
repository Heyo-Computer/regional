//! Shared layout, styling and small rendering helpers.
//!
//! Server-rendered with maud, which escapes interpolated values by default —
//! worth having when a page echoes text that anyone on the internet typed
//! into the public form. Nothing is loaded from a CDN, so both pages work on
//! a locked-down network and inside a VM with no egress.

use maud::{DOCTYPE, Markup, PreEscaped, html};
use regional_core::submission::{Request, Status};

pub const STYLE: &str = r#"
:root {
  --bg: #fbfbfa; --surface: #ffffff; --surface-2: #f4f4f2;
  --text: #1a1c1a; --muted: #6a6f6b; --border: #e3e5e1;
  --accent: #2f6f4e; --accent-text: #ffffff; --accent-soft: #e8f2ec;
  --warn: #8a6100; --warn-soft: #fdf3dd;
  --info: #1d4f91; --info-soft: #e6eefa;
  --ok: #2f6f4e; --ok-soft: #e8f2ec;
  --off: #6a6f6b; --off-soft: #eeefec;
  --radius: 10px;
}
@media (prefers-color-scheme: dark) {
  :root {
    --bg: #14171a; --surface: #1b1f23; --surface-2: #22272c;
    --text: #e7e9e6; --muted: #98a09a; --border: #2b3137;
    --accent: #6fbf8f; --accent-text: #10221a; --accent-soft: #1d2a23;
    --warn: #e0b05a; --warn-soft: #2a2418;
    --info: #7fb0ea; --info-soft: #18222f;
    --ok: #6fbf8f; --ok-soft: #1d2a23;
    --off: #98a09a; --off-soft: #23282c;
  }
}
* { box-sizing: border-box; }
body {
  margin: 0; background: var(--bg); color: var(--text);
  font: 15px/1.55 ui-sans-serif, system-ui, -apple-system, "Segoe UI", Roboto,
        "Helvetica Neue", Arial, sans-serif;
  -webkit-font-smoothing: antialiased;
}
.wrap { max-width: 68rem; margin: 0 auto; padding: 0 16px 64px; }
.wrap.narrow { max-width: 44rem; }
header.top {
  border-bottom: 1px solid var(--border); background: var(--surface);
  margin-bottom: 28px;
}
header.top .wrap { padding-top: 16px; padding-bottom: 16px; display: flex;
  gap: 16px; align-items: baseline; flex-wrap: wrap; }
header.top .brand { font-weight: 650; letter-spacing: -0.01em; }
header.top .brand a { color: var(--text); text-decoration: none; }
header.top nav { margin-left: auto; display: flex; gap: 4px; flex-wrap: wrap; }
header.top nav a {
  color: var(--muted); text-decoration: none; padding: 4px 10px;
  border-radius: 999px; font-size: 14px;
}
header.top nav a:hover { background: var(--surface-2); color: var(--text); }
header.top nav a[aria-current] { background: var(--accent-soft); color: var(--accent); font-weight: 600; }
h1 { font-size: 24px; letter-spacing: -0.02em; margin: 0 0 6px; }
h2 { font-size: 17px; letter-spacing: -0.01em; margin: 28px 0 10px; }
h3 { font-size: 15px; margin: 0 0 8px; }
p { margin: 0 0 12px; }
a { color: var(--accent); }
.lede { color: var(--muted); margin-bottom: 24px; max-width: 54ch; }
.muted { color: var(--muted); }
.small { font-size: 13px; }
.mono { font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace; font-size: 13px; }

.grid { display: grid; gap: 12px; grid-template-columns: repeat(auto-fit, minmax(190px, 1fr)); }
.card {
  background: var(--surface); border: 1px solid var(--border);
  border-radius: var(--radius); padding: 14px 16px;
}
.card .n { font-size: 26px; font-weight: 650; letter-spacing: -0.02em; }
.card .k { color: var(--muted); font-size: 13px; text-transform: uppercase;
  letter-spacing: 0.04em; }
.card .sub { color: var(--muted); font-size: 13px; margin-top: 2px; }
.card a.n { text-decoration: none; }

table { width: 100%; border-collapse: collapse; background: var(--surface);
  border: 1px solid var(--border); border-radius: var(--radius); overflow: hidden; }
th, td { text-align: left; padding: 9px 12px; border-bottom: 1px solid var(--border);
  vertical-align: top; }
th { font-size: 12px; text-transform: uppercase; letter-spacing: 0.04em;
  color: var(--muted); background: var(--surface-2); font-weight: 600; }
tr:last-child td { border-bottom: none; }
td.nowrap, th.nowrap { white-space: nowrap; }

.badge {
  display: inline-block; padding: 1px 8px; border-radius: 999px;
  font-size: 12px; font-weight: 600; white-space: nowrap;
}
.badge.pending  { background: var(--warn-soft); color: var(--warn); }
.badge.approved { background: var(--info-soft); color: var(--info); }
.badge.applied  { background: var(--ok-soft);   color: var(--ok); }
.badge.rejected { background: var(--off-soft);  color: var(--off); }
.badge.plain    { background: var(--surface-2); color: var(--muted); }

.chips { display: flex; gap: 6px; flex-wrap: wrap; margin-bottom: 16px; }
.chips a {
  text-decoration: none; font-size: 13px; padding: 4px 11px; border-radius: 999px;
  border: 1px solid var(--border); color: var(--muted); background: var(--surface);
}
.chips a[aria-current] { background: var(--accent); border-color: var(--accent);
  color: var(--accent-text); font-weight: 600; }

form.stack { display: grid; gap: 14px; }
label { display: block; font-weight: 600; font-size: 14px; margin-bottom: 4px; }
label .opt { font-weight: 400; color: var(--muted); }
.hint { color: var(--muted); font-size: 13px; margin: 4px 0 0; }
input[type=text], input[type=url], input[type=email], input[type=number],
textarea, select {
  width: 100%; padding: 9px 11px; font: inherit; color: var(--text);
  background: var(--surface); border: 1px solid var(--border);
  border-radius: 8px;
}
textarea { min-height: 110px; resize: vertical; }
input:focus, textarea:focus, select:focus {
  outline: 2px solid var(--accent); outline-offset: 1px; border-color: var(--accent);
}
fieldset { border: 1px solid var(--border); border-radius: var(--radius);
  padding: 12px 14px; margin: 0; background: var(--surface); }
legend { font-weight: 600; font-size: 14px; padding: 0 6px; }
.radios { display: grid; gap: 8px; }
.radios label { display: flex; gap: 9px; align-items: flex-start; font-weight: 400; }
.radios input { margin-top: 3px; }
.radios .t { font-weight: 600; }
.row { display: grid; gap: 14px; grid-template-columns: 1fr 1fr; }
@media (max-width: 34rem) { .row { grid-template-columns: 1fr; } }

button, .btn {
  font: inherit; font-weight: 600; padding: 9px 16px; border-radius: 8px;
  border: 1px solid var(--accent); background: var(--accent);
  color: var(--accent-text); cursor: pointer; text-decoration: none;
  display: inline-block;
}
button.secondary, .btn.secondary {
  background: var(--surface); color: var(--text); border-color: var(--border);
}
button:hover, .btn:hover { filter: brightness(1.06); }
.actions { display: flex; gap: 10px; flex-wrap: wrap; align-items: center; }

.note { border-left: 3px solid var(--accent); background: var(--accent-soft);
  padding: 10px 14px; border-radius: 0 8px 8px 0; margin: 0 0 18px; }
.note.warn { border-color: var(--warn); background: var(--warn-soft); }
.note.err  { border-color: #b3261e; background: var(--warn-soft); }

dl.facts { display: grid; grid-template-columns: max-content 1fr; gap: 6px 18px;
  margin: 0 0 18px; }
dl.facts dt { color: var(--muted); font-size: 13px; }
dl.facts dd { margin: 0; }
.hp { position: absolute; left: -9999px; width: 1px; height: 1px; overflow: hidden; }
footer.foot { color: var(--muted); font-size: 13px; border-top: 1px solid var(--border);
  margin-top: 40px; padding-top: 16px; }
"#;

pub struct Nav {
    pub items: Vec<(&'static str, &'static str)>,
    pub current: &'static str,
}

pub fn page(brand: &str, brand_href: &str, nav: Option<&Nav>, title: &str, body: Markup) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { (title) }
                style { (PreEscaped(STYLE)) }
            }
            body {
                header.top {
                    .wrap {
                        .brand { a href=(brand_href) { (brand) } }
                        @if let Some(nav) = nav {
                            nav {
                                @for (href, label) in &nav.items {
                                    @if *label == nav.current {
                                        a href=(href) aria-current="page" { (label) }
                                    } @else {
                                        a href=(href) { (label) }
                                    }
                                }
                            }
                        }
                    }
                }
                (body)
            }
        }
    }
}

pub fn status_badge(s: Status) -> Markup {
    html! { span class={ "badge " (s.as_str()) } { (s.as_str()) } }
}

pub fn request_badge(r: Request) -> Markup {
    html! { span."badge"."plain" { (r.as_str()) } }
}

/// Absolute UTC, which is what an operator reconciling logs actually wants.
pub fn ts(t: i64) -> String {
    chrono::DateTime::from_timestamp(t, 0)
        .map(|d| d.format("%Y-%m-%d %H:%M UTC").to_string())
        .unwrap_or_else(|| "—".into())
}

/// "4 minutes ago" — the at-a-glance form, paired with the absolute one.
pub fn ago(t: i64) -> String {
    let secs = (regional_core::model::now_ts() - t).max(0);
    let (n, unit) = match secs {
        s if s < 60 => return "just now".into(),
        s if s < 3_600 => (s / 60, "minute"),
        s if s < 86_400 => (s / 3_600, "hour"),
        s if s < 2_592_000 => (s / 86_400, "day"),
        s => (s / 2_592_000, "month"),
    };
    format!("{n} {unit}{} ago", if n == 1 { "" } else { "s" })
}

/// "1 URL" / "2 URLs". Small thing, but a dashboard that says "1 URLs"
/// reads like nobody looked at it.
pub fn plural(n: usize, word: &str) -> String {
    format!("{n} {word}{}", if n == 1 { "" } else { "s" })
}

pub fn opt_ts(t: Option<i64>) -> String {
    t.map(ts).unwrap_or_else(|| "—".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interpolated_text_is_escaped() {
        // The public form echoes whatever anyone types; maud must escape it.
        let body = html! { p { "<script>alert(1)</script>" } };
        let out = page("B", "/", None, "T", body).into_string();
        assert!(out.contains("&lt;script&gt;"), "{out}");
        assert!(!out.contains("<script>alert"));
    }

    #[test]
    fn relative_times_read_naturally() {
        let now = regional_core::model::now_ts();
        assert_eq!(ago(now), "just now");
        assert_eq!(ago(now - 60), "1 minute ago");
        assert_eq!(ago(now - 7_200), "2 hours ago");
        assert_eq!(ago(now - 86_400 * 3), "3 days ago");
        // A clock skew that puts a timestamp in the future must not underflow.
        assert_eq!(ago(now + 5_000), "just now");
    }

    #[test]
    fn counts_are_pluralised() {
        assert_eq!(plural(0, "URL"), "0 URLs");
        assert_eq!(plural(1, "URL"), "1 URL");
        assert_eq!(plural(2, "error"), "2 errors");
    }

    #[test]
    fn timestamps_render_or_dash() {
        assert_eq!(ts(0), "1970-01-01 00:00 UTC");
        assert_eq!(opt_ts(None), "—");
    }

    #[test]
    fn the_page_declares_a_theme_for_both_colour_schemes() {
        let out = page("B", "/", None, "T", html! {}).into_string();
        assert!(out.contains("prefers-color-scheme: dark"));
        assert!(out.contains("viewport"));
    }
}
