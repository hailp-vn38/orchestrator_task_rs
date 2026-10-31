//! Bộ phân tích thẻ, thuộc tính, HTML entities, làm sạch nhiễu và chuẩn hóa URL thuần Rust.

/// Giải mã các HTML entities phổ biến và mã số (`&#...;`, `&#x...;`).
pub fn decode_html_entities(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let after = &rest[amp + 1..];
        let Some(semi_rel) = after.bytes().take(12).position(|b| b == b';') else {
            out.push('&');
            rest = after;
            continue;
        };
        let entity = &after[..semi_rel];
        let decoded = match entity {
            "nbsp" | "#160" => Some(" ".to_string()),
            "amp" => Some("&".to_string()),
            "lt" => Some("<".to_string()),
            "gt" => Some(">".to_string()),
            "quot" => Some("\"".to_string()),
            "apos" | "#39" | "#039" => Some("'".to_string()),
            "ndash" => Some("–".to_string()),
            "mdash" => Some("—".to_string()),
            "laquo" => Some("«".to_string()),
            "raquo" => Some("»".to_string()),
            "hellip" => Some("…".to_string()),
            _ if entity.starts_with("#x") || entity.starts_with("#X") => {
                u32::from_str_radix(&entity[2..], 16)
                    .ok()
                    .and_then(char::from_u32)
                    .map(|c| c.to_string())
            }
            _ if entity.starts_with('#') => entity[1..]
                .parse::<u32>()
                .ok()
                .and_then(char::from_u32)
                .map(|c| c.to_string()),
            _ => None,
        };
        if let Some(ch) = decoded {
            out.push_str(&ch);
            rest = &after[semi_rel + 1..];
        } else {
            out.push('&');
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

/// Gộp mọi chuỗi khoảng trắng liên tiếp thành một dấu cách đơn.
pub fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Bỏ toàn bộ thẻ HTML trong đoạn văn bản và giải mã entity.
pub fn strip_tags(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' => {
                in_tag = false;
                out.push(' ');
            }
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    collapse_ws(&decode_html_entities(&out))
}

fn is_tag_boundary(b: Option<u8>) -> bool {
    matches!(b, None | Some(b' ' | b'\t' | b'\r' | b'\n' | b'>' | b'/'))
}

fn find_tag_end(html: &str, start: usize) -> Option<usize> {
    html[start..].find('>').map(|i| start + i + 1)
}

/// Xóa comment HTML `<!-- ... -->`.
pub fn remove_html_comments(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(start) = rest.find("<!--") {
        out.push_str(&rest[..start]);
        if let Some(end) = rest[start + 4..].find("-->") {
            rest = &rest[start + 4 + end + 3..];
        } else {
            return out;
        }
    }
    out.push_str(rest);
    out
}

/// Xóa mọi khối `<tag ...>...</tag>` (có tính độ sâu lồng nhau).
pub fn remove_tag_blocks(html: &str, tag: &str) -> String {
    let lower = html.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let open_pat = format!("<{tag}");
    let close_pat = format!("</{tag}");
    let mut out = String::with_capacity(html.len());
    let mut cursor = 0usize;

    while cursor < html.len() {
        let Some(rel) = lower[cursor..].find(&open_pat) else {
            out.push_str(&html[cursor..]);
            break;
        };
        let start = cursor + rel;
        if !is_tag_boundary(bytes.get(start + open_pat.len()).copied()) {
            out.push_str(&html[cursor..start + open_pat.len()]);
            cursor = start + open_pat.len();
            continue;
        }
        out.push_str(&html[cursor..start]);
        let Some(mut scan) = find_tag_end(&lower, start) else {
            break;
        };
        if lower[start..scan].trim_end().ends_with("/>") {
            cursor = scan;
            continue;
        }
        let mut depth = 1usize;
        while depth > 0 && scan < html.len() {
            let next_open = lower[scan..].find(&open_pat).map(|i| scan + i);
            let next_close = lower[scan..].find(&close_pat).map(|i| scan + i);
            match (next_open, next_close) {
                (Some(o), Some(c)) if o < c => {
                    if is_tag_boundary(bytes.get(o + open_pat.len()).copied()) {
                        if let Some(end_o) = find_tag_end(&lower, o) {
                            if !lower[o..end_o].trim_end().ends_with("/>") {
                                depth += 1;
                            }
                            scan = end_o;
                            continue;
                        }
                    }
                    scan = o + open_pat.len();
                }
                (_, Some(c)) => {
                    if is_tag_boundary(bytes.get(c + close_pat.len()).copied()) {
                        depth -= 1;
                        scan = find_tag_end(&lower, c).unwrap_or(html.len());
                    } else {
                        scan = c + close_pat.len();
                    }
                }
                _ => {
                    scan = html.len();
                    break;
                }
            }
        }
        cursor = scan;
    }
    out
}

/// Tìm các phần tử `<tag ...>inner</tag>` (trả về `(open_tag_attrs, inner_html)`).
pub fn find_elements(html: &str, tag: &str) -> Vec<(String, String)> {
    let lower = html.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let open_pat = format!("<{tag}");
    let close_pat = format!("</{tag}");
    let mut results = Vec::new();
    let mut cursor = 0usize;

    while cursor < html.len() {
        let Some(rel) = lower[cursor..].find(&open_pat) else {
            break;
        };
        let start = cursor + rel;
        if !is_tag_boundary(bytes.get(start + open_pat.len()).copied()) {
            cursor = start + open_pat.len();
            continue;
        }
        let Some(open_end) = find_tag_end(&lower, start) else {
            break;
        };
        let open_tag = html[start + open_pat.len()..open_end - 1].to_string();
        if open_tag.trim_end().ends_with('/') {
            results.push((open_tag, String::new()));
            cursor = open_end;
            continue;
        }
        let mut scan = open_end;
        let mut depth = 1usize;
        let mut close_start = html.len();
        let mut after_close = html.len();
        while depth > 0 && scan < html.len() {
            let next_open = lower[scan..].find(&open_pat).map(|i| scan + i);
            let next_close = lower[scan..].find(&close_pat).map(|i| scan + i);
            match (next_open, next_close) {
                (Some(o), Some(c)) if o < c => {
                    if is_tag_boundary(bytes.get(o + open_pat.len()).copied()) {
                        if let Some(end_o) = find_tag_end(&lower, o) {
                            if !lower[o..end_o].trim_end().ends_with("/>") {
                                depth += 1;
                            }
                            scan = end_o;
                            continue;
                        }
                    }
                    scan = o + open_pat.len();
                }
                (_, Some(c)) => {
                    if is_tag_boundary(bytes.get(c + close_pat.len()).copied()) {
                        depth -= 1;
                        close_start = c;
                        after_close = find_tag_end(&lower, c).unwrap_or(html.len());
                        scan = after_close;
                    } else {
                        scan = c + close_pat.len();
                    }
                }
                _ => break,
            }
        }
        let inner = html[open_end..close_start].to_string();
        results.push((open_tag, inner));
        cursor = after_close;
    }
    results
}

/// Lấy giá trị thuộc tính `attr="..."` hoặc `attr='...'` từ chuỗi thuộc tính của thẻ.
pub fn get_attr(attrs: &str, name: &str) -> Option<String> {
    let lower = attrs.to_ascii_lowercase();
    let mut search_from = 0usize;
    while search_from < lower.len() {
        let Some(rel) = lower[search_from..].find(name) else {
            break;
        };
        let pos = search_from + rel;
        let before_ok = pos == 0
            || matches!(
                lower.as_bytes()[pos - 1],
                b' ' | b'\t' | b'\r' | b'\n' | b'"' | b'\''
            );
        let after_name = &attrs[pos + name.len()..];
        let trimmed = after_name.trim_start();
        if !before_ok || !trimmed.starts_with('=') {
            search_from = pos + name.len();
            continue;
        }
        let val_part = trimmed[1..].trim_start();
        let val = if let Some(rest) = val_part.strip_prefix('"') {
            rest.split('"').next().unwrap_or("")
        } else if let Some(rest) = val_part.strip_prefix('\'') {
            rest.split('\'').next().unwrap_or("")
        } else {
            val_part.split_whitespace().next().unwrap_or("")
        };
        return Some(decode_html_entities(val.trim()));
    }
    None
}

/// Tìm tất cả các thẻ đơn/mở `<tag ...>` và trả về chuỗi thuộc tính bên trong.
pub fn find_open_tags(html: &str, tag: &str) -> Vec<String> {
    let lower = html.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let pat = format!("<{tag}");
    let mut out = Vec::new();
    let mut cursor = 0usize;
    while let Some(rel) = lower[cursor..].find(&pat) {
        let start = cursor + rel;
        if !is_tag_boundary(bytes.get(start + pat.len()).copied()) {
            cursor = start + pat.len();
            continue;
        }
        let Some(end) = find_tag_end(&lower, start) else {
            break;
        };
        out.push(html[start + pat.len()..end - 1].to_string());
        cursor = end;
    }
    out
}

/// Làm sạch các thẻ nhiễu (script, style, svg, nav, footer, form, iframe, button).
pub fn clean_noise_tags(html: &str) -> String {
    let mut cleaned = remove_html_comments(html);
    for noisy in [
        "script", "style", "noscript", "svg", "nav", "footer", "aside", "form", "iframe", "button",
    ] {
        cleaned = remove_tag_blocks(&cleaned, noisy);
    }
    cleaned
}

/// Chuẩn hóa URL liên kết (hỗ trợ URL tương đối `/path` hoặc `//host/path`).
pub fn resolve_url(base_url: &str, href: &str) -> Option<String> {
    let href = href.trim();
    if href.is_empty()
        || href.starts_with('#')
        || href.starts_with("javascript:")
        || href.starts_with("mailto:")
        || href.starts_with("tel:")
    {
        return None;
    }
    if href.starts_with("http://") || href.starts_with("https://") {
        return Some(href.to_string());
    }
    let (scheme, rest) = base_url.split_once("://")?;
    if href.starts_with("//") {
        return Some(format!("{scheme}:{href}"));
    }
    let host = rest.split('/').next()?;
    if href.starts_with('/') {
        return Some(format!("{scheme}://{host}{href}"));
    }
    Some(format!("{scheme}://{host}/{href}"))
}
