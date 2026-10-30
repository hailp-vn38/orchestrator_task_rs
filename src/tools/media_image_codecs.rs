//! Giải mã ảnh tham chiếu (`data:image/...;base64,...` / `ArtifactStore`) và bộ kết xuất đồ họa
//! SVG / PNG chuẩn không phụ thuộc thư viện ngoài.

use orchestrator_agent_rs::{ToolContext, ToolError};
use serde_json::Value;

use crate::tools::media_artifacts::{MAX_IMAGE_REFERENCES, MAX_REFERENCE_IMAGE_BYTES};
use crate::tools::media_image::ReferenceImageInput;
use crate::tools::wiki_bridge::host_tool_err;

pub(crate) async fn resolve_image_references_from_args(
    ctx: Option<&ToolContext>,
    args: &Value,
) -> Result<Vec<ReferenceImageInput>, ToolError> {
    let mut out = Vec::new();

    if let Some(arr) = args.get("reference_artifact_ids").and_then(Value::as_array) {
        if arr.len() > MAX_IMAGE_REFERENCES {
            return Err(host_tool_err(
                "TOO_MANY_REFERENCE_IMAGES",
                format!("At most {MAX_IMAGE_REFERENCES} reference images are allowed"),
            ));
        }
        let tool_ctx = ctx.ok_or_else(|| {
            host_tool_err(
                "ARTIFACT_UNAVAILABLE",
                "ToolContext is required to resolve reference_artifact_ids",
            )
        })?;
        for item in arr {
            let id = item
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| {
                    host_tool_err(
                        "INVALID_ARGUMENT",
                        "reference_artifact_ids entries must be non-empty strings",
                    )
                })?;
            out.push(resolve_artifact_image(tool_ctx, id).await?);
        }
    }

    if let Some(arr) = args.get("reference_images").and_then(Value::as_array) {
        for item in arr {
            let raw = item
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| {
                    host_tool_err(
                        "INVALID_ARGUMENT",
                        "reference_images entries must be non-empty data URL or base64 strings",
                    )
                })?;
            out.push(decode_data_url_or_base64_image(raw)?);
        }
    }

    if out.len() > MAX_IMAGE_REFERENCES {
        return Err(host_tool_err(
            "TOO_MANY_REFERENCE_IMAGES",
            format!("At most {MAX_IMAGE_REFERENCES} reference images are allowed"),
        ));
    }

    Ok(out)
}

pub(crate) async fn resolve_first_frame_from_args(
    ctx: Option<&ToolContext>,
    args: &Value,
) -> Result<Option<ReferenceImageInput>, ToolError> {
    let artifact_id = args
        .get("first_frame_artifact_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let inline_img = args
        .get("first_frame_image")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());

    if artifact_id.is_some() && inline_img.is_some() {
        return Err(host_tool_err(
            "INVALID_ARGUMENT",
            "Specify at most one of first_frame_artifact_id or first_frame_image",
        ));
    }

    if let Some(id) = artifact_id {
        let tool_ctx = ctx.ok_or_else(|| {
            host_tool_err(
                "ARTIFACT_UNAVAILABLE",
                "ToolContext is required to resolve first_frame_artifact_id",
            )
        })?;
        return Ok(Some(resolve_artifact_image(tool_ctx, id).await?));
    }

    if let Some(raw) = inline_img {
        return Ok(Some(decode_data_url_or_base64_image(raw)?));
    }

    Ok(None)
}

async fn resolve_artifact_image(
    ctx: &ToolContext,
    artifact_id: &str,
) -> Result<ReferenceImageInput, ToolError> {
    let reader = ctx.artifacts().open(artifact_id).await?;
    let meta = reader.metadata();
    if meta.size_bytes > MAX_REFERENCE_IMAGE_BYTES {
        return Err(host_tool_err(
            "REFERENCE_IMAGE_TOO_LARGE",
            format!(
                "Reference artifact '{}' exceeds limit of {} bytes",
                artifact_id, MAX_REFERENCE_IMAGE_BYTES
            ),
        ));
    }
    let declared_mime = meta.mime_type.clone();
    let bytes = reader
        .read_all()
        .await
        .map_err(|e| host_tool_err("ARTIFACT_INTEGRITY", e.to_string()))?;
    let mime_type = sniff_raster_image_mime(&bytes).ok_or_else(|| {
        host_tool_err(
            "UNSUPPORTED_REFERENCE_IMAGE",
            format!(
                "Artifact '{}' (mime={}) is not a supported raster image (png/jpeg/webp/gif)",
                artifact_id, declared_mime
            ),
        )
    })?;
    Ok(ReferenceImageInput {
        bytes,
        mime_type: mime_type.to_string(),
    })
}

fn decode_data_url_or_base64_image(raw: &str) -> Result<ReferenceImageInput, ToolError> {
    let trimmed = raw.trim();
    let (declared_mime, b64_part) = if let Some(rest) = trimmed.strip_prefix("data:") {
        let Some((header, payload)) = rest.split_once(',') else {
            return Err(host_tool_err("INVALID_ARGUMENT", "Malformed data URL"));
        };
        let mime = header
            .split(';')
            .next()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        (mime, payload.trim())
    } else {
        (None, trimmed)
    };

    let bytes = decode_base64(b64_part)
        .map_err(|e| host_tool_err("INVALID_ARGUMENT", format!("Invalid base64 image: {e}")))?;
    if bytes.is_empty() {
        return Err(host_tool_err(
            "INVALID_ARGUMENT",
            "Reference image bytes cannot be empty",
        ));
    }
    if (bytes.len() as u64) > MAX_REFERENCE_IMAGE_BYTES {
        return Err(host_tool_err(
            "REFERENCE_IMAGE_TOO_LARGE",
            format!(
                "Reference image exceeds limit of {} bytes",
                MAX_REFERENCE_IMAGE_BYTES
            ),
        ));
    }
    let mime_type = sniff_raster_image_mime(&bytes).ok_or_else(|| {
        host_tool_err(
            "UNSUPPORTED_REFERENCE_IMAGE",
            "Reference image must be a valid raster image (png, jpeg, webp, or gif)",
        )
    })?;
    let _ = declared_mime;
    Ok(ReferenceImageInput {
        bytes,
        mime_type: mime_type.to_string(),
    })
}

fn sniff_raster_image_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        return Some("image/png");
    }
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Some("image/jpeg");
    }
    if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some("image/gif");
    }
    None
}

#[cfg(test)]
pub(crate) fn encode_base64(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0];
        let b1 = chunk.get(1).copied().unwrap_or(0);
        let b2 = chunk.get(2).copied().unwrap_or(0);
        out.push(TABLE[(b0 >> 2) as usize] as char);
        out.push(TABLE[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize] as char);
        if chunk.len() > 1 {
            out.push(TABLE[(((b1 & 0x0f) << 2) | (b2 >> 6)) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(TABLE[(b2 & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

pub(crate) fn decode_base64(input: &str) -> Result<Vec<u8>, String> {
    let mut sextets = Vec::with_capacity(input.len());
    let mut pad = 0usize;
    for b in input.bytes() {
        if b.is_ascii_whitespace() {
            continue;
        }
        if b == b'=' {
            pad += 1;
            continue;
        }
        if pad > 0 {
            return Err("unexpected character after base64 padding".to_string());
        }
        let val = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            _ => return Err("invalid base64 character".to_string()),
        };
        sextets.push(val);
    }
    let mut out = Vec::with_capacity((sextets.len() * 3) / 4);
    let mut i = 0;
    while i + 4 <= sextets.len() {
        let s0 = sextets[i];
        let s1 = sextets[i + 1];
        let s2 = sextets[i + 2];
        let s3 = sextets[i + 3];
        out.push((s0 << 2) | (s1 >> 4));
        out.push((s1 << 4) | (s2 >> 2));
        out.push((s2 << 6) | s3);
        i += 4;
    }
    let rem = sextets.len() - i;
    if rem == 1 {
        return Err("invalid base64 length".to_string());
    } else if rem == 2 {
        let s0 = sextets[i];
        let s1 = sextets[i + 1];
        out.push((s0 << 2) | (s1 >> 4));
    } else if rem == 3 {
        let s0 = sextets[i];
        let s1 = sextets[i + 1];
        let s2 = sextets[i + 2];
        out.push((s0 << 2) | (s1 >> 4));
        out.push((s1 << 4) | (s2 >> 2));
    }
    Ok(out)
}

fn xml_escape(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(ch),
        }
    }
    out
}

pub(crate) fn extract_svg_root_dimensions(raw_svg: &str) -> Option<(u32, u32)> {
    let trimmed = raw_svg.trim();
    if !trimmed.get(..4)?.eq_ignore_ascii_case("<svg") {
        return None;
    }
    let tag_end = trimmed.find('>')?;
    let open_tag = &trimmed[4..tag_end];
    let parse_dim = |s: &str| -> Option<u32> {
        let digits: String = s
            .trim()
            .trim_end_matches("px")
            .trim()
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == '.')
            .collect();
        digits
            .parse::<f64>()
            .ok()
            .filter(|&v| v >= 16.0)
            .map(|v| (v.round() as u32).clamp(16, 2048))
    };
    let attr_w = crate::tools::web::get_attr(open_tag, "width").and_then(|v| parse_dim(&v));
    let attr_h = crate::tools::web::get_attr(open_tag, "height").and_then(|v| parse_dim(&v));
    if let (Some(w), Some(h)) = (attr_w, attr_h) {
        return Some((w, h));
    }
    if let Some(vb) = crate::tools::web::get_attr(open_tag, "viewbox") {
        let nums: Vec<f64> = vb
            .split(|c: char| c.is_whitespace() || c == ',')
            .filter(|s| !s.is_empty())
            .filter_map(|s| s.parse::<f64>().ok())
            .collect();
        if nums.len() == 4 && nums[2] >= 16.0 && nums[3] >= 16.0 {
            let w = attr_w.unwrap_or_else(|| (nums[2].round() as u32).clamp(16, 2048));
            let h = attr_h.unwrap_or_else(|| (nums[3].round() as u32).clamp(16, 2048));
            return Some((w, h));
        }
    }
    None
}

pub(crate) fn ensure_svg_root_dimensions(
    raw_svg: &str,
    width: u32,
    height: u32,
) -> (String, u32, u32) {
    if let Some((svg_w, svg_h)) = extract_svg_root_dimensions(raw_svg) {
        return (raw_svg.to_string(), svg_w, svg_h);
    }
    let trimmed = raw_svg.trim();
    if trimmed.len() >= 4 && trimmed[..4].eq_ignore_ascii_case("<svg") {
        let injected = format!(
            r#"<svg width="{width}" height="{height}" viewBox="0 0 {width} {height}"{}"#,
            &trimmed[4..]
        );
        return (injected, width, height);
    }
    (trimmed.to_string(), width, height)
}

fn wrap_prompt_lines(prompt: &str, max_chars: usize, max_lines: usize) -> Vec<String> {
    let mut lines = Vec::new();
    for para in prompt.lines().map(str::trim).filter(|s| !s.is_empty()) {
        let mut current = String::new();
        for word in para.split_whitespace() {
            if !current.is_empty() && current.chars().count() + 1 + word.chars().count() > max_chars
            {
                lines.push(xml_escape(&current));
                current.clear();
                if lines.len() >= max_lines {
                    return lines;
                }
            }
            if !current.is_empty() {
                current.push(' ');
            }
            current.push_str(word);
        }
        if !current.is_empty() && lines.len() < max_lines {
            lines.push(xml_escape(&current));
        }
        if lines.len() >= max_lines {
            break;
        }
    }
    lines
}

pub(crate) fn render_svg_card(
    width: u32,
    height: u32,
    title: &str,
    prompt: &str,
    custom_body: Option<&str>,
) -> String {
    let safe_title = xml_escape(title);
    let body_markup = match custom_body {
        Some(custom) if !custom.trim().is_empty() => {
            let safe_prompt = xml_escape(&prompt.chars().take(160).collect::<String>());
            format!(
                r##"  <text x="32" y="92" fill="#94a3b8" font-family="sans-serif" font-size="14">{safe_prompt}</text>
  {custom}"##
            )
        }
        _ => {
            let max_chars = ((width.saturating_sub(96)) / 9).clamp(32, 110) as usize;
            let lines = wrap_prompt_lines(prompt, max_chars, 10);
            let box_w = width.saturating_sub(64).max(64);
            let box_h = height.saturating_sub(116).max(64);
            let tspans: String = lines
                .iter()
                .enumerate()
                .map(|(i, line)| {
                    format!(
                        "    <tspan x=\"52\" y=\"{}\">{line}</tspan>\n",
                        132 + (i as u32) * 24
                    )
                })
                .collect();
            format!(
                r##"  <rect x="32" y="88" width="{box_w}" height="{box_h}" rx="12" fill="#1e293b" stroke="#334155" stroke-width="1.5"/>
  <text fill="#e2e8f0" font-family="sans-serif" font-size="15">
{tspans}  </text>"##
            )
        }
    };
    format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}" viewBox="0 0 {width} {height}">
  <defs>
    <linearGradient id="bg" x1="0%" y1="0%" x2="100%" y2="100%">
      <stop offset="0%" stop-color="#0f172a"/>
      <stop offset="100%" stop-color="#1e293b"/>
    </linearGradient>
  </defs>
  <rect width="100%" height="100%" rx="16" fill="url(#bg)"/>
  <text x="32" y="56" fill="#f8fafc" font-family="sans-serif" font-size="24" font-weight="bold">{safe_title}</text>
{body_markup}
</svg>
"##
    )
}

/// Kết xuất một ảnh PNG chuẩn (8-bit RGB, uncompressed zlib blocks + Adler32 + CRC32) không cần thư viện ngoài.
pub(crate) fn encode_gradient_png(width: u32, height: u32, seed_text: &str) -> Vec<u8> {
    let mut seed: u32 = 0x811c_9dc5;
    for b in seed_text.bytes() {
        seed ^= u32::from(b);
        seed = seed.wrapping_mul(0x0100_0193);
    }
    let r_base = (seed & 0xff) as u8;
    let g_base = ((seed >> 8) & 0xff) as u8;
    let b_base = ((seed >> 16) & 0xff) as u8;

    // Mỗi dòng scanline gồm 1 filter byte (0) + width * 3 bytes RGB
    let row_len = 1 + (width as usize) * 3;
    let mut raw_scanlines = Vec::with_capacity(row_len * (height as usize));
    for y in 0..height {
        raw_scanlines.push(0u8); // Filter type 0 (None)
        for x in 0..width {
            let r = r_base.wrapping_add(((x * 255) / width.max(1)) as u8);
            let g = g_base.wrapping_add(((y * 255) / height.max(1)) as u8);
            let b = b_base;
            raw_scanlines.extend_from_slice(&[r, g, b]);
        }
    }

    let zlib_stream = wrap_zlib_stored(&raw_scanlines);

    let mut png = Vec::with_capacity(64 + zlib_stream.len());
    // PNG signature
    png.extend_from_slice(&[137, 80, 78, 71, 13, 10, 26, 10]);

    // IHDR chunk (width, height, bit_depth=8, color_type=2 RGB, compression=0, filter=0, interlace=0)
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
    write_png_chunk(&mut png, b"IHDR", &ihdr);

    // IDAT chunk
    write_png_chunk(&mut png, b"IDAT", &zlib_stream);

    // IEND chunk
    write_png_chunk(&mut png, b"IEND", &[]);
    png
}

fn wrap_zlib_stored(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + (data.len() / 65535 + 1) * 5 + 6);
    // Zlib header: CMF=0x78 (deflate, 32K window), FLG=0x01 (no dict, check bits)
    out.extend_from_slice(&[0x78, 0x01]);

    let chunks: Vec<&[u8]> = if data.is_empty() {
        vec![&[]]
    } else {
        data.chunks(65_535).collect()
    };
    for (i, chunk) in chunks.iter().enumerate() {
        let is_last = i + 1 == chunks.len();
        out.push(if is_last { 0x01 } else { 0x00 });
        let len = chunk.len() as u16;
        let nlen = !len;
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&nlen.to_le_bytes());
        out.extend_from_slice(chunk);
    }

    let adler = adler32(data);
    out.extend_from_slice(&adler.to_be_bytes());
    out
}

fn adler32(data: &[u8]) -> u32 {
    let mut a: u32 = 1;
    let mut b: u32 = 0;
    for &byte in data {
        a = (a + u32::from(byte)) % 65_521;
        b = (b + a) % 65_521;
    }
    (b << 16) | a
}

fn crc32(chunk_type: &[u8; 4], chunk_data: &[u8]) -> u32 {
    let mut crc: u32 = 0xffff_ffff;
    for &byte in chunk_type.iter().chain(chunk_data.iter()) {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            if crc & 1 != 0 {
                crc = (crc >> 1) ^ 0xedb8_8320;
            } else {
                crc >>= 1;
            }
        }
    }
    !crc
}

fn write_png_chunk(out: &mut Vec<u8>, chunk_type: &[u8; 4], chunk_data: &[u8]) {
    let len = chunk_data.len() as u32;
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(chunk_type);
    out.extend_from_slice(chunk_data);
    let crc = crc32(chunk_type, chunk_data);
    out.extend_from_slice(&crc.to_be_bytes());
}
