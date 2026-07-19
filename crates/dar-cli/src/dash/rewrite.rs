//! Heuristic HTML rewriter for pages proxied under `/agent/<port>/`.

use std::sync::LazyLock;

use regex::Regex;

static PROTECTED: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?is)<!--.*?-->|<style\b[^>]*>.*?</style>").unwrap());
static ATTRS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)((?:^|[\s<])(?:href|src|action|formaction|poster|data-src|data-href|hx-get|hx-post|hx-put|hx-patch|hx-delete|data-tab-url)\s*=\s*[\"'])(/[^/\"'][^\"']*)"#).unwrap()
});
static SCRIPTS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?is)(<script\b[^>]*>)(.*?)(</script>)").unwrap());
static STRINGS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?:\"(/[^/\"][^\"]*)\"|'(/[^/'][^']*)'|(^|[=(,:;\s])`(/[^/`][^`]*)`)"#).unwrap()
});
static DOUBLE_QUOTED_HANDLERS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?is)((?:^|[\s<])on[\w:-]*\s*=\s*\")(.*?)\""#).unwrap());
static SINGLE_QUOTED_HANDLERS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?is)((?:^|[\s<])on[\w:-]*\s*=\s*')(.*?)'").unwrap());

// Compatibility shim for third-party / non-prefix-aware pages; first-party
// frontends opt out via the `x-prefix-aware` response header. This is
// deliberately heuristic: it rewrites dar's HTML attributes and inline
// JavaScript string literals, not arbitrary JavaScript syntax. Known
// limitation: bare `href="/"` is intentionally left unrewritten — an
// agent-root link falls through to the fleet shell.
pub(super) fn rewrite_html(html: &str, port: u16) -> String {
    let prefix = format!("/agent/{port}");
    let mut rewritten = String::with_capacity(html.len());
    let mut end = 0;
    for block in PROTECTED.find_iter(html) {
        rewritten.push_str(&rewrite_html_fragment(&html[end..block.start()], &prefix));
        rewritten.push_str(block.as_str());
        end = block.end();
    }
    rewritten.push_str(&rewrite_html_fragment(&html[end..], &prefix));
    rewritten
}

fn rewrite_html_fragment(html: &str, prefix: &str) -> String {
    let rewritten = ATTRS.replace_all(html, |captures: &regex::Captures<'_>| {
        format!("{}{}{}", &captures[1], prefix, &captures[2])
    });
    let rewrite_strings = |code: &str| {
        STRINGS
            .replace_all(code, |string: &regex::Captures<'_>| {
                if let Some(path) = string.get(1) {
                    format!("\"{prefix}{}\"", path.as_str())
                } else if let Some(path) = string.get(2) {
                    format!("'{prefix}{}'", path.as_str())
                } else {
                    format!("{}{}{}{}{}", &string[3], '`', prefix, &string[4], '`')
                }
            })
            .into_owned()
    };
    let rewritten = SCRIPTS.replace_all(&rewritten, |captures: &regex::Captures<'_>| {
        format!(
            "{}{}{}",
            &captures[1],
            rewrite_strings(&captures[2]),
            &captures[3]
        )
    });
    let rewritten =
        DOUBLE_QUOTED_HANDLERS.replace_all(&rewritten, |captures: &regex::Captures<'_>| {
            format!("{}{}\"", &captures[1], rewrite_strings(&captures[2]))
        });
    SINGLE_QUOTED_HANDLERS
        .replace_all(&rewritten, |captures: &regex::Captures<'_>| {
            format!("{}{}'", &captures[1], rewrite_strings(&captures[2]))
        })
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrites_url_attributes() {
        let html = r#"<script src="/assets/x.js"></script><button hx-post="/control/pause"></button><a data-tab-url="/scheduler/tab"></a>"#;
        let rewritten = rewrite_html(html, 50123);
        assert!(rewritten.contains("src=\"/agent/50123/assets/x.js\""));
        assert!(rewritten.contains("hx-post=\"/agent/50123/control/pause\""));
        assert!(rewritten.contains("data-tab-url=\"/agent/50123/scheduler/tab\""));
    }

    #[test]
    fn rewrites_script_strings_and_backticks() {
        let html = r#"<script>
  app.es = new EventSource(`/chat/${SESSION}/stream`);
  fetch(`/chat/${SESSION}/send`, { method: 'POST' });
  fetch(`/static/no-interp`);
  fetch(`//cdn.example/x`);
  const markdown = s => s.replace(/```x```/g, () => `ok`);
</script><script>fetch('/content'); new EventSource('/events')</script>"#;
        let rewritten = rewrite_html(html, 50123);
        assert!(rewritten.contains("EventSource(`/agent/50123/chat/${SESSION}/stream`)"));
        assert!(rewritten.contains("fetch(`/agent/50123/chat/${SESSION}/send`"));
        assert!(rewritten.contains("fetch(`/agent/50123/static/no-interp`)"));
        assert!(rewritten.contains("`//cdn.example/x`"));
        assert!(rewritten.contains("/```x```/g"));
        assert!(rewritten.contains("fetch('/agent/50123/content')"));
        assert!(rewritten.contains("EventSource('/agent/50123/events')"));
    }

    #[test]
    fn rewrites_inline_handlers() {
        let html = r#"<button onclick="fetch('/run-now')"></button><button onclick="fetch('/scheduler/jobs/abc/run-now')"></button><button onclick='fetch("/scheduler/jobs/abc/run-now")'></button>"#;
        let rewritten = rewrite_html(html, 50123);
        assert!(rewritten.contains("onclick=\"fetch('/agent/50123/run-now')\""));
        assert!(rewritten.contains("onclick=\"fetch('/agent/50123/scheduler/jobs/abc/run-now')\""));
        assert!(rewritten.contains("onclick='fetch(\"/agent/50123/scheduler/jobs/abc/run-now\")'"));
    }

    #[test]
    fn protects_comment_and_style_blocks() {
        let html = r#"<style>.x { background: url('/style') }</style><!-- <a href="/comment"> -->"#;
        let rewritten = rewrite_html(html, 50123);
        assert!(rewritten.contains("url('/style')"));
        assert!(rewritten.contains("href=\"/comment\""));
    }

    #[test]
    fn leaves_non_dashboard_urls_alone() {
        let html = r#"<a href="https://example.com/x"></a><img src="//cdn.example/x"><a href="page.html"></a><a href="/"></a><div foo-href="/not-an-attribute"></div>"#;
        let rewritten = rewrite_html(html, 50123);
        assert!(rewritten.contains("https://example.com/x"));
        assert!(rewritten.contains("//cdn.example/x"));
        assert!(rewritten.contains("href=\"page.html\""));
        // Bare href="/" is intentionally left unrewritten: a known limitation
        // of the heuristic shim (extending the regexes would need lookahead or
        // risk clobbering protocol-relative //host URLs); an agent-root link
        // falls through to the fleet shell.
        assert!(rewritten.contains("href=\"/\""), "{rewritten}");
        assert!(rewritten.contains("foo-href=\"/not-an-attribute\""));
    }
}
