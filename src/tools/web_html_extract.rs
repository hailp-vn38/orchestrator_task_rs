//! Các hàm bóc tách ngữ nghĩa cấp cao từ trang HTML (metadata, bài viết, liên kết, chuyên mục).

use std::collections::HashMap;

use serde_json::{json, Value};

use crate::tools::web_html_parser::{
    clean_noise_tags, decode_html_entities, find_elements, find_open_tags, get_attr, resolve_url,
    strip_tags,
};

/// Trích xuất metadata từ HTML (`<title>`, `<meta>`, `<link rel="canonical">`, JSON-LD).
pub fn extract_metadata(html: &str) -> Value {
    let title = find_elements(html, "title")
        .into_iter()
        .next()
        .map(|(_, inner)| strip_tags(&inner))
        .unwrap_or_default();

    let mut meta_map: HashMap<String, String> = HashMap::new();
    for attrs in find_open_tags(html, "meta") {
        let key = get_attr(&attrs, "property")
            .or_else(|| get_attr(&attrs, "name"))
            .or_else(|| get_attr(&attrs, "itemprop"));
        let content = get_attr(&attrs, "content");
        if let (Some(k), Some(v)) = (key, content) {
            if !v.is_empty() {
                meta_map.entry(k.to_ascii_lowercase()).or_insert(v);
            }
        }
    }

    let canonical = find_open_tags(html, "link")
        .into_iter()
        .find(|a| get_attr(a, "rel").is_some_and(|r| r.eq_ignore_ascii_case("canonical")))
        .and_then(|a| get_attr(&a, "href"));

    let mut ld_headlines = Vec::new();
    let mut ld_descriptions = Vec::new();
    let mut ld_authors = Vec::new();
    let mut ld_dates = Vec::new();
    for (attrs, inner) in find_elements(html, "script") {
        let is_ld =
            get_attr(&attrs, "type").is_some_and(|t| t.to_ascii_lowercase().contains("ld+json"));
        if !is_ld {
            continue;
        }
        if let Ok(val) = serde_json::from_str::<Value>(inner.trim()) {
            if let Some(h) = val.get("headline").and_then(Value::as_str) {
                ld_headlines.push(decode_html_entities(h.trim()));
            }
            if let Some(d) = val.get("description").and_then(Value::as_str) {
                ld_descriptions.push(decode_html_entities(d.trim()));
            }
            if let Some(dt) = val.get("datePublished").and_then(Value::as_str) {
                ld_dates.push(dt.trim().to_string());
            }
            if let Some(author) = val.get("author") {
                if let Some(name) = author.get("name").and_then(Value::as_str) {
                    ld_authors.push(decode_html_entities(name.trim()));
                } else if let Some(s) = author.as_str() {
                    ld_authors.push(decode_html_entities(s.trim()));
                }
            }
        }
    }

    let origin_data_src = find_elements(html, "span")
        .into_iter()
        .find(|(attrs, _)| {
            get_attr(attrs, "id")
                .is_some_and(|id| id == "url_origin_full" || id == "origin_name_full")
        })
        .and_then(|(attrs, _)| get_attr(&attrs, "data-src"));

    json!({
        "title": if !title.is_empty() { Some(title) } else { meta_map.get("og:title").cloned() },
        "og_title": meta_map.get("og:title"),
        "description": meta_map.get("description").or_else(|| meta_map.get("og:description")),
        "site_name": meta_map.get("og:site_name"),
        "keywords": meta_map.get("keywords").or_else(|| meta_map.get("news_keywords")),
        "canonical_url": canonical.or_else(|| meta_map.get("og:url").cloned()),
        "published_at": meta_map.get("pubdate")
            .or_else(|| meta_map.get("datepublished"))
            .cloned()
            .or_else(|| ld_dates.into_iter().next()),
        "author": meta_map.get("author").cloned().or_else(|| ld_authors.into_iter().next()),
        "ld_headline": ld_headlines.into_iter().next(),
        "ld_description": ld_descriptions.into_iter().next(),
        "origin_data_src": origin_data_src,
    })
}

/// Chọn vùng HTML chứa nội dung chính của bài viết (`<article>`, `<main>`, hoặc `<body>`).
pub fn locate_article_region(html: &str) -> String {
    let articles = find_elements(html, "article");
    if let Some((_, inner)) = articles.iter().find(|(attrs, _)| {
        get_attr(attrs, "id").is_some_and(|id| id.contains("article"))
            || get_attr(attrs, "class").is_some_and(|c| c.contains("content-main"))
    }) {
        return inner.clone();
    }
    if let Some((_, inner)) = articles.into_iter().find(|(_, inner)| inner.len() > 200) {
        return inner;
    }
    if let Some((_, inner)) = find_elements(html, "main").into_iter().next() {
        return inner;
    }
    if let Some((_, inner)) = find_elements(html, "body").into_iter().next() {
        return inner;
    }
    html.to_string()
}

/// Kết quả bóc tách bài viết có cấu trúc từ HTML.
#[derive(Debug, Clone)]
pub struct ExtractedArticle {
    pub title: String,
    pub sapo: String,
    pub sections: Vec<String>,
    pub images: Vec<Value>,
}

/// Trích xuất bài viết có cấu trúc từ HTML.
pub fn extract_article(html: &str) -> ExtractedArticle {
    let meta = extract_metadata(html);
    let region = clean_noise_tags(&locate_article_region(html));

    let h1_title = find_elements(&region, "h1")
        .into_iter()
        .map(|(_, inner)| strip_tags(&inner))
        .find(|t| !t.is_empty())
        .or_else(|| {
            find_elements(html, "h1")
                .into_iter()
                .map(|(_, inner)| strip_tags(&inner))
                .find(|t| !t.is_empty())
        })
        .or_else(|| {
            meta.get("title")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_default();

    let sapo = find_elements(&region, "h2")
        .into_iter()
        .map(|(_, inner)| strip_tags(&inner))
        .find(|t| !t.is_empty())
        .or_else(|| {
            meta.get("description")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_default();

    let mut sections: Vec<String> = Vec::new();
    let mut push_unique = |line: String| {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return;
        }
        if trimmed == "Lưu bài viết thành công"
            || trimmed.starts_with("Bạn có thể xem lại bài viết đã lưu")
            || trimmed == "Bấm xem >>"
        {
            return;
        }
        if !sections.iter().any(|existing| existing == trimmed) {
            sections.push(trimmed.to_string());
        }
    };

    for (attrs, inner) in find_elements(&region, "h2") {
        let text = strip_tags(&inner);
        let is_sapo = get_attr(&attrs, "id").is_some_and(|id| id.contains("sapo")) || text == sapo;
        if !text.is_empty() && !is_sapo {
            push_unique(format!("## {text}"));
        }
    }
    for (_, inner) in find_elements(&region, "h3") {
        let text = strip_tags(&inner);
        if !text.is_empty() {
            push_unique(format!("### {text}"));
        }
    }
    for (_, inner) in find_elements(&region, "p") {
        let text = strip_tags(&inner);
        if !text.is_empty() && text != h1_title && text != sapo {
            push_unique(text);
        }
    }
    for (_, inner) in find_elements(&region, "li") {
        let text = strip_tags(&inner);
        if !text.is_empty() {
            push_unique(format!("- {text}"));
        }
    }

    let mut images = Vec::new();
    for attrs in find_open_tags(&region, "img") {
        let url = get_attr(&attrs, "data-original")
            .or_else(|| get_attr(&attrs, "data-src"))
            .or_else(|| get_attr(&attrs, "src"))
            .unwrap_or_default();
        if url.is_empty()
            || url.starts_with("data:image/")
            || url.contains("in-image-close")
            || url.contains("logo")
            || url.contains("loading.gif")
        {
            continue;
        }
        let alt = get_attr(&attrs, "alt").unwrap_or_default();
        images.push(json!({"alt": alt, "url": url}));
    }

    ExtractedArticle {
        title: h1_title,
        sapo,
        sections,
        images,
    }
}

/// Trích xuất các liên kết `<a href="...">text</a>` từ vùng bài viết hoặc toàn trang.
pub fn extract_links(html: &str, base_url: &str, article_only: bool, limit: usize) -> Vec<Value> {
    let target = if article_only {
        clean_noise_tags(&locate_article_region(html))
    } else {
        clean_noise_tags(html)
    };
    let mut links = Vec::new();
    let mut seen = Vec::new();
    for (attrs, inner) in find_elements(&target, "a") {
        let Some(raw_href) = get_attr(&attrs, "href") else {
            continue;
        };
        let Some(url) = resolve_url(base_url, &raw_href) else {
            continue;
        };
        let text = strip_tags(&inner);
        let title_attr = get_attr(&attrs, "title").unwrap_or_default();
        let label = if !text.is_empty() && text != "Bấm xem >>" {
            text
        } else if !title_attr.is_empty() {
            title_attr
        } else {
            continue;
        };
        if seen.iter().any(|u| u == &url) {
            continue;
        }
        seen.push(url.clone());
        links.push(json!({"text": label, "url": url}));
        if links.len() >= limit {
            break;
        }
    }
    links
}

/// Trích xuất danh sách các mục tin (`title`, `url`, `snippet`) trên trang chuyên mục / trang chủ.
pub fn extract_feed_items(html: &str, base_url: &str, limit: usize) -> Vec<Value> {
    let cleaned = clean_noise_tags(html);
    let mut items = Vec::new();
    let mut seen_urls = Vec::new();

    for (_, article_inner) in find_elements(&cleaned, "article") {
        let mut found_title = String::new();
        let mut found_url = String::new();

        for h_tag in ["h1", "h2", "h3", "h4"] {
            for (_, h_inner) in find_elements(&article_inner, h_tag) {
                for (a_attrs, a_inner) in find_elements(&h_inner, "a") {
                    if let Some(href) =
                        get_attr(&a_attrs, "href").and_then(|h| resolve_url(base_url, &h))
                    {
                        let t = strip_tags(&a_inner);
                        let title = if !t.is_empty() {
                            t
                        } else {
                            get_attr(&a_attrs, "title").unwrap_or_default()
                        };
                        if !title.is_empty() {
                            found_title = title;
                            found_url = href;
                            break;
                        }
                    }
                }
                if !found_url.is_empty() {
                    break;
                }
            }
            if !found_url.is_empty() {
                break;
            }
        }

        if found_url.is_empty() || seen_urls.iter().any(|u| u == &found_url) {
            continue;
        }

        let snippet = find_elements(&article_inner, "p")
            .into_iter()
            .map(|(_, p_inner)| strip_tags(&p_inner))
            .find(|text| !text.is_empty() && text != &found_title)
            .unwrap_or_default();

        seen_urls.push(found_url.clone());
        items.push(json!({
            "title": found_title,
            "url": found_url,
            "snippet": snippet,
        }));
        if items.len() >= limit {
            break;
        }
    }
    items
}
