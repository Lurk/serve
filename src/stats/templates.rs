const SETUP_HTML: &str = include_str!("templates/setup.html");
const LOGIN_HTML: &str = include_str!("templates/login.html");

#[must_use]
pub fn render_setup(csrf: &str, error: Option<&str>, prefix: &str) -> String {
    render(SETUP_HTML, csrf, error, prefix)
}

#[must_use]
pub fn render_login(csrf: &str, error: Option<&str>, prefix: &str) -> String {
    render(LOGIN_HTML, csrf, error, prefix)
}

fn render(template: &str, csrf: &str, error: Option<&str>, prefix: &str) -> String {
    let mut out = template.replace("{{CSRF}}", &html_escape(csrf));
    let err_html = error.map_or_else(String::new, |msg| {
        format!(r#"<p class="err">{}</p>"#, html_escape(msg))
    });
    out = out.replace("<!--ERROR-->", &err_html);
    // Prefix comes from validated config, but escape defensively in case that
    // ever changes.
    out = out.replace("{{PREFIX}}", &html_escape(prefix));
    out
}

fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

const DASHBOARD_HTML: &str = include_str!("templates/dashboard.html");

#[must_use]
pub fn render_dashboard(prefix: &str) -> String {
    DASHBOARD_HTML.replace("{{PREFIX}}", &html_escape(prefix))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn setup_injects_csrf_and_no_error() {
        let html = render_setup("tok-abc", None, "/__stats__");
        assert!(html.contains(r#"value="tok-abc""#));
        assert!(!html.contains(r#"class="err""#));
    }

    #[test]
    fn setup_renders_error() {
        let html = render_setup("t", Some("password mismatch"), "/__stats__");
        assert!(html.contains("password mismatch"));
    }

    #[test]
    fn html_escape_protects_against_injection() {
        let html = render_login("<script>", None, "/__stats__");
        assert!(!html.contains("<script>"));
        assert!(html.contains("&lt;script&gt;"));
    }

    #[test]
    fn dashboard_includes_all_panels() {
        let h = render_dashboard("/__stats__");
        for needle in [
            "chart-req",
            "chart-bytes",
            "id=\"assets\"",
            "id=\"cards\"",
            "logout",
        ] {
            assert!(h.contains(needle), "dashboard missing {needle}");
        }
    }

    #[test]
    fn render_uses_custom_prefix() {
        let h = render_dashboard("/admin/stats");
        assert!(h.contains("'/admin/stats'"));
        assert!(!h.contains("'/__stats__'"));
        let l = render_login("c", None, "/admin/stats");
        assert!(l.contains("/admin/stats/login"));
    }
}
