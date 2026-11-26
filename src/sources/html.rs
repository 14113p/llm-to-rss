use regex::Regex;

/// Strip scripts/styles/tags and decode common HTML entities into readable plain text.
pub fn html_to_text(html: &str) -> String {
    let script_re =
        Regex::new(r"(?is)<(script|style|noscript|svg)[^>]*>.*?</(script|style|noscript|svg)>")
            .expect("valid regex");
    let without_scripts = script_re.replace_all(html, " ");

    let tag_re = Regex::new(r"(?s)<[^>]+>").expect("valid regex");
    let without_tags = tag_re.replace_all(&without_scripts, " ");

    let decoded = without_tags
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ");

    let ws_re = Regex::new(r"\s+").expect("valid regex");
    ws_re.replace_all(decoded.trim(), " ").into_owned()
}

pub fn truncate_chars(text: &str, max_chars: usize) -> String {
    let mut chars = text.chars();
    let collected: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{collected}… [truncated]")
    } else {
        collected
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_html_scripts_and_tags() {
        let html = "<html><head><style>body{}</style><script>alert(1)</script></head><body><h1>PS5 &amp; Pro</h1><p>Price: <b>$449</b></p></body></html>";
        assert_eq!(html_to_text(html), "PS5 & Pro Price: $449");
    }

    #[test]
    fn truncates_cleanly() {
        assert_eq!(truncate_chars("abcdef", 4), "abcd… [truncated]");
        assert_eq!(truncate_chars("abc", 4), "abc");
    }
}
