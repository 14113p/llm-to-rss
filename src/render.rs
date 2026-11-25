use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use pulldown_cmark::{html, Options, Parser};

use crate::config::FeedConfig;
use crate::error::{AppError, Result};
use crate::fetcher::FetchedSource;
use crate::store::{Citation, FeedEntry};

/// Render a feed prompt by substituting template variables and attaching pre-fetched sources.
pub fn render_prompt(
    feed: &FeedConfig,
    now_local: DateTime<Tz>,
    sources: &[FetchedSource],
) -> String {
    let date_str = now_local.format("%Y-%m-%d").to_string();
    let datetime_str = now_local.to_rfc3339();

    let sources_block = if sources.is_empty() {
        String::new()
    } else {
        sources
            .iter()
            .map(|s| format!("### {} ({})\n{}", s.name, s.url, s.content))
            .collect::<Vec<_>>()
            .join("\n\n")
    };

    let has_sources_placeholder = feed.prompt.contains("{{sources}}");
    let mut rendered = feed
        .prompt
        .replace("{{date}}", &date_str)
        .replace("{{datetime}}", &datetime_str)
        .replace("{{feed_title}}", &feed.title)
        .replace("{{feed_id}}", &feed.id)
        .replace("{{sources}}", &sources_block);

    if !sources.is_empty() && !has_sources_placeholder {
        rendered.push_str("\n\n---\n## Pre-fetched Sources\n\n");
        rendered.push_str(&sources_block);
    }

    rendered
}

/// Extract an optional leading `# Headline` from the LLM markdown, resolve the entry title,
/// and convert markdown + citations into HTML suitable for RSS/Atom feed readers.
pub fn render_entry_html(
    feed: &FeedConfig,
    now_local: DateTime<Tz>,
    raw_markdown: &str,
    citations: &[Citation],
) -> (String, String, String) {
    let trimmed = raw_markdown.trim();
    let (headline, body_md) = split_leading_heading(trimmed);

    let date_str = now_local.format("%Y-%m-%d").to_string();
    let datetime_str = now_local.format("%Y-%m-%d %H:%M %Z").to_string();

    let title = if let Some(tmpl) = &feed.entry_title_template {
        tmpl.replace("{{date}}", &date_str)
            .replace("{{datetime}}", &datetime_str)
            .replace("{{feed_title}}", &feed.title)
            .replace("{{headline}}", headline.as_deref().unwrap_or(&feed.title))
    } else if let Some(h) = headline {
        h
    } else {
        format!("{} — {}", feed.title, date_str)
    };

    let mut html_out = markdown_to_html(body_md);

    if !citations.is_empty() {
        html_out.push_str("<hr/><h3>Sources</h3><ul>");
        for c in citations {
            html_out.push_str(&format!(
                "<li><a href=\"{}\" rel=\"noopener noreferrer\">{}</a></li>",
                escape_xml(&c.url),
                escape_xml(&c.title)
            ));
        }
        html_out.push_str("</ul>");
    }

    let summary = plain_summary(body_md, 280);
    (title, html_out, summary)
}

fn split_leading_heading(md: &str) -> (Option<String>, &str) {
    let mut lines = md.lines();
    if let Some(first) = lines.next() {
        let stripped = first.trim();
        if let Some(heading) = stripped.strip_prefix("# ") {
            let h = heading.trim();
            if !h.is_empty() {
                let rest = md[first.len()..].trim_start_matches(['\r', '\n']);
                return (Some(h.to_string()), rest);
            }
        }
    }
    (None, md)
}

pub fn markdown_to_html(md: &str) -> String {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TASKLISTS);
    let parser = Parser::new_ext(md, options);
    let mut html_buf = String::new();
    html::push_html(&mut html_buf, parser);
    html_buf
}

fn plain_summary(md: &str, max_chars: usize) -> String {
    let text = md
        .lines()
        .map(|l| l.trim_start_matches('#').trim())
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    let mut chars = text.chars();
    let clipped: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{clipped}…")
    } else {
        clipped
    }
}

/// Serialize a virtual feed and its stored entries as Atom 1.0 XML.
pub fn build_atom_feed(feed: &FeedConfig, base_url: &str, entries: &[FeedEntry]) -> Result<String> {
    let base = base_url.trim_end_matches('/');
    let self_url = format!("{base}/feeds/{}.xml", feed.id);
    let updated: DateTime<Utc> = entries
        .first()
        .map(|e| e.published_at)
        .unwrap_or_else(Utc::now);

    let atom_entries: Vec<atom_syndication::Entry> = entries
        .iter()
        .map(|e| {
            let mut content = atom_syndication::Content::default();
            content.set_content_type(Some("html".to_string()));
            content.set_value(Some(e.content_html.clone()));

            let mut link = atom_syndication::Link::default();
            link.set_href(e.link.clone());
            link.set_rel("alternate".to_string());

            let mut author = atom_syndication::Person::default();
            author.set_name(format!("llm-to-rss ({})", e.model));

            let mut summary = atom_syndication::Text::plain(e.summary.clone());
            summary.lang = None;

            atom_syndication::EntryBuilder::default()
                .id(e.id.clone())
                .title(e.title.clone())
                .updated(e.published_at)
                .published(Some(e.published_at.into()))
                .summary(Some(summary))
                .content(Some(content))
                .links(vec![link])
                .authors(vec![author])
                .build()
        })
        .collect();

    let mut self_link = atom_syndication::Link::default();
    self_link.set_href(self_url.clone());
    self_link.set_rel("self".to_string());

    let mut builder = atom_syndication::FeedBuilder::default();
    builder
        .id(self_url)
        .title(feed.title.clone())
        .updated(updated)
        .links(vec![self_link])
        .entries(atom_entries);

    if let Some(desc) = &feed.description {
        builder.subtitle(Some(atom_syndication::Text::plain(desc.clone())));
    }

    let atom_feed = builder.build();
    Ok(atom_feed.to_string())
}

/// Serialize a virtual feed and its stored entries as RSS 2.0 XML.
pub fn build_rss_feed(feed: &FeedConfig, base_url: &str, entries: &[FeedEntry]) -> Result<String> {
    let base = base_url.trim_end_matches('/');
    let self_url = format!("{base}/feeds/{}.rss", feed.id);

    let items: Vec<rss::Item> = entries
        .iter()
        .map(|e| {
            let mut guid = rss::Guid::default();
            guid.set_value(e.id.clone());
            guid.set_permalink(false);

            rss::ItemBuilder::default()
                .title(Some(e.title.clone()))
                .link(Some(e.link.clone()))
                .guid(Some(guid))
                .pub_date(Some(e.published_at.to_rfc2822()))
                .description(Some(e.content_html.clone()))
                .author(Some(format!("llm-to-rss ({})", e.model)))
                .build()
        })
        .collect();

    let channel = rss::ChannelBuilder::default()
        .title(feed.title.clone())
        .link(self_url)
        .description(
            feed.description
                .clone()
                .unwrap_or_else(|| format!("Virtual LLM feed: {}", feed.title)),
        )
        .generator(Some("llm-to-rss".to_string()))
        .items(items)
        .build();

    let mut buf = Vec::new();
    channel
        .write_to(&mut buf)
        .map_err(|e| AppError::Serde(e.to_string()))?;
    String::from_utf8(buf).map_err(|e| AppError::Serde(e.to_string()))
}

/// Build an OPML 2.0 subscription file grouped by feed category for one-click RSS reader import.
pub fn build_opml(feeds: &[FeedConfig], base_url: &str) -> String {
    let base = base_url.trim_end_matches('/');
    let mut grouped: BTreeMap<String, Vec<&FeedConfig>> = BTreeMap::new();
    for feed in feeds.iter().filter(|f| f.enabled) {
        let cat = feed
            .category
            .clone()
            .unwrap_or_else(|| "Virtual Feeds".to_string());
        grouped.entry(cat).or_default().push(feed);
    }

    let mut out = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <opml version=\"2.0\">\n\
           <head>\n\
             <title>llm-to-rss Virtual Feeds</title>\n\
           </head>\n\
           <body>\n",
    );

    for (category, cat_feeds) in grouped {
        out.push_str(&format!(
            "    <outline text=\"{}\" title=\"{}\">\n",
            escape_xml(&category),
            escape_xml(&category)
        ));
        for f in cat_feeds {
            let xml_url = format!("{base}/feeds/{}.xml", f.id);
            let html_url = format!("{base}/");
            out.push_str(&format!(
                "      <outline type=\"rss\" text=\"{}\" title=\"{}\" xmlUrl=\"{}\" htmlUrl=\"{}\"/>\n",
                escape_xml(&f.title),
                escape_xml(&f.title),
                escape_xml(&xml_url),
                escape_xml(&html_url),
            ));
        }
        out.push_str("    </outline>\n");
    }

    out.push_str("  </body>\n</opml>\n");
    out
}

pub fn escape_xml(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::FeedConfig;

    fn sample_feed() -> FeedConfig {
        FeedConfig {
            id: "morning".into(),
            title: "Morning Briefing".into(),
            description: Some("Daily digest".into()),
            category: Some("Daily".into()),
            enabled: true,
            schedule: "0 7 * * *".into(),
            timezone: None,
            provider: "openai".into(),
            model: None,
            web_search: None,
            system_prompt: None,
            prompt: "Summarize news for {{date}}".into(),
            entry_title_template: None,
            sources: vec![],
            skip_if_contains: None,
            deduplicate: false,
            max_items: None,
            run_on_startup: false,
        }
    }

    #[test]
    fn renders_markdown_with_leading_heading_and_citations() {
        let feed = sample_feed();
        let tz: Tz = "UTC".parse().unwrap();
        let now = Utc::now().with_timezone(&tz);
        let citations = vec![Citation {
            title: "Example".into(),
            url: "https://example.com".into(),
        }];
        let (title, html, _summary) = render_entry_html(
            &feed,
            now,
            "# Today's Briefing\n\n- **Item 1**\n- Item 2",
            &citations,
        );
        assert_eq!(title, "Today's Briefing");
        assert!(html.contains("<strong>Item 1</strong>"));
        assert!(html.contains("https://example.com"));
    }

    #[test]
    fn generates_valid_atom_rss_and_opml() {
        let feed = sample_feed();
        let entry = FeedEntry {
            id: "urn:uuid:1".into(),
            feed_id: "morning".into(),
            title: "Test Entry".into(),
            link: "http://localhost:8080/feeds/morning.xml".into(),
            content_html: "<p>Hello</p>".into(),
            summary: "Hello".into(),
            published_at: Utc::now(),
            content_hash: "abc".into(),
            model: "gpt-4o".into(),
            citations: vec![],
        };

        let atom_xml =
            build_atom_feed(&feed, "http://localhost:8080", std::slice::from_ref(&entry)).unwrap();
        assert!(atom_syndication::Feed::read_from(atom_xml.as_bytes()).is_ok());

        let rss_xml = build_rss_feed(&feed, "http://localhost:8080", &[entry]).unwrap();
        assert!(rss::Channel::read_from(rss_xml.as_bytes()).is_ok());

        let opml = build_opml(&[feed], "http://localhost:8080");
        assert!(opml.contains("xmlUrl=\"http://localhost:8080/feeds/morning.xml\""));
    }
}
