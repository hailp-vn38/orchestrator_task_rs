//! Chuyển đổi tài liệu PDF/Office sang GitHub-Flavored Markdown bằng `anydoc`,
//! chuẩn hóa bố cục Markdown và tạo PDF kỹ thuật mẫu phục vụ kiểm thử.

use std::io;
use std::path::Path;

use anydoc::{ConvertError, Format};
use llm_wiki::slugify;

use crate::tools::pdf::ConvertedPdfDoc;

fn format_label(format: Format) -> &'static str {
    match format {
        Format::Pdf => "pdf",
        Format::Doc => "doc",
        Format::Docx => "docx",
        Format::Odt => "odt",
        Format::Ppt => "ppt",
        Format::Pptx => "pptx",
        Format::Rtf => "rtf",
        Format::Epub => "epub",
        Format::Excel => "excel",
        Format::Ods => "ods",
        Format::Odp => "odp",
        Format::Csv => "csv",
    }
}

/// Chuyển đổi một tài liệu PDF/Office tại `path` sang GitHub-Flavored Markdown bằng `anydoc`.
pub fn convert_document_with_anydoc(
    index: usize,
    path: &Path,
    max_chars: Option<usize>,
) -> io::Result<ConvertedPdfDoc> {
    let bytes = std::fs::read(path).map_err(|e| {
        io::Error::new(
            e.kind(),
            format!("Không thể đọc file `{}`: {e}", path.display()),
        )
    })?;
    if bytes.is_empty() {
        return Err(io::Error::other(format!("File rỗng: {}", path.display())));
    }

    let format = Format::from_bytes(&bytes)
        .or_else(|| Format::from_path(path))
        .ok_or_else(|| {
            io::Error::other(format!(
                "anydoc không nhận diện được định dạng của file `{}`",
                path.display()
            ))
        })?;

    let (raw_markdown, skipped_ocr_pages, total_pdf_pages) =
        extract_markdown_with_anydoc(&bytes, format, path)?;

    let raw_file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("document.pdf")
        .to_string();
    let raw_stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("document");
    let clean_stem = slugify(raw_stem, 48);
    let file_stem = if clean_stem.is_empty() {
        format!("doc-{index:02}")
    } else {
        clean_stem
    };

    let (normalized_md, inferred_title, outline_headings) =
        normalize_anydoc_markdown(&raw_markdown, raw_stem, max_chars);
    let char_count = normalized_md.chars().count();
    let line_count = normalized_md.lines().count().max(1);

    Ok(ConvertedPdfDoc {
        index,
        source_path: path.to_path_buf(),
        file_name: raw_file_name,
        file_stem,
        format_name: format_label(format),
        inferred_title,
        outline_headings,
        markdown: normalized_md,
        char_count,
        line_count,
        skipped_ocr_pages,
        total_pdf_pages,
        saved_source_scoped_path: None,
    })
}

fn extract_markdown_with_anydoc(
    bytes: &[u8],
    format: Format,
    path: &Path,
) -> io::Result<(String, Vec<u32>, Option<u32>)> {
    match anydoc::to_markdown_bytes(bytes, format) {
        Ok(md) => Ok((md, Vec::new(), None)),
        Err(ConvertError::NeedsOcr { pages, page_count }) if format == Format::Pdf => {
            if pages.len() >= page_count as usize {
                return Err(io::Error::other(format!(
                    "Toàn bộ {page_count} trang của PDF `{}` là ảnh scan (cần OCR)",
                    path.display()
                )));
            }

            if let Ok(mut doc) = lopdf::Document::load_mem(bytes) {
                doc.delete_pages(&pages);
                let mut stripped_buf = Vec::new();
                if doc.save_to(&mut stripped_buf).is_ok() {
                    if let Ok(md) = anydoc::to_markdown_bytes(&stripped_buf, Format::Pdf) {
                        let note = format!(
                            "> *[Lưu ý từ `anydoc`: Đã trích xuất văn bản từ {}/{} trang PDF; bỏ qua {} trang thuần ảnh: {:?}]*\n\n",
                            page_count.saturating_sub(pages.len() as u32),
                            page_count,
                            pages.len(),
                            pages
                        );
                        return Ok((format!("{note}{md}"), pages, Some(page_count)));
                    }
                }
            }

            let inspected = pdf_inspector::process_pdf_mem(bytes).map_err(|e| {
                io::Error::other(format!(
                    "Không thể trích xuất trang văn bản từ `{}`: {e}",
                    path.display()
                ))
            })?;
            if let Some(md) = inspected.markdown.filter(|s| !s.trim().is_empty()) {
                let note = format!(
                    "> *[Lưu ý từ `anydoc`: Đã trích xuất văn bản từ {}/{} trang PDF; bỏ qua {} trang thuần ảnh: {:?}]*\n\n",
                    page_count.saturating_sub(pages.len() as u32),
                    page_count,
                    pages.len(),
                    pages
                );
                return Ok((format!("{note}{md}"), pages, Some(page_count)));
            }

            Err(io::Error::other(format!(
                "Lỗi khi chuyển đổi `{}` sang Markdown bằng anydoc: pages {:?} of {} need OCR",
                path.display(),
                pages,
                page_count
            )))
        }
        Err(e) => Err(io::Error::other(format!(
            "Lỗi khi chuyển đổi `{}` sang Markdown bằng anydoc: {e}",
            path.display()
        ))),
    }
}

fn normalize_anydoc_markdown(
    raw_md: &str,
    fallback_stem: &str,
    max_chars: Option<usize>,
) -> (String, String, Vec<String>) {
    let mut inferred_title: Option<String> = None;
    let mut headings: Vec<String> = Vec::new();
    let mut lines_out: Vec<String> = Vec::new();
    let mut in_fence = false;
    let mut blank_streak = 0usize;

    for raw_line in raw_md.lines() {
        let trimmed_end = raw_line.trim_end();
        let trimmed = trimmed_end.trim();

        if trimmed.starts_with("```") {
            in_fence = !in_fence;
            blank_streak = 0;
            lines_out.push(trimmed_end.to_string());
            continue;
        }
        if in_fence {
            lines_out.push(trimmed_end.to_string());
            continue;
        }
        if trimmed.is_empty() {
            blank_streak += 1;
            if blank_streak <= 1 && !lines_out.is_empty() {
                lines_out.push(String::new());
            }
            continue;
        }
        blank_streak = 0;

        if trimmed.starts_with('#') {
            let level = trimmed.chars().take_while(|&c| c == '#').count();
            let text = trimmed[level..].trim();
            if !text.is_empty() {
                if inferred_title.is_none() && level <= 2 {
                    inferred_title = Some(text.to_string());
                }
                if level <= 3 && headings.len() < 25 {
                    headings.push(format!("{} {}", "#".repeat(level), text));
                }
            }
            lines_out.push(trimmed.to_string());
            continue;
        }

        if trimmed.starts_with('|') || trimmed.chars().count() <= 140 {
            lines_out.push(trimmed_end.to_string());
        } else {
            for wrapped in wrap_long_prose_line(trimmed, 110) {
                lines_out.push(wrapped);
            }
        }
    }

    let mut joined = lines_out.join("\n");
    if let Some(limit) = max_chars.filter(|&n| n > 0) {
        if joined.chars().count() > limit {
            let mut cut = String::new();
            for line in joined.lines() {
                if cut.chars().count() + line.chars().count() + 1 > limit {
                    break;
                }
                cut.push_str(line);
                cut.push('\n');
            }
            cut.push_str(&format!(
                "\n> *[Nội dung PDF được cắt tại ngưỡng {limit} ký tự]*\n"
            ));
            joined = cut;
        }
    }

    let title = inferred_title.unwrap_or_else(|| humanize_stem(fallback_stem));
    (joined, title, headings)
}

fn wrap_long_prose_line(line: &str, max_width: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut current_chars = 0usize;

    for word in line.split_whitespace() {
        let w_len = word.chars().count();
        if current_chars > 0 && current_chars + 1 + w_len > max_width {
            out.push(std::mem::take(&mut current));
            current_chars = 0;
        }
        if current_chars > 0 {
            current.push(' ');
            current_chars += 1;
        }
        current.push_str(word);
        current_chars += w_len;
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

fn humanize_stem(stem: &str) -> String {
    let words: Vec<String> = stem
        .split(|c: char| c == '-' || c == '_' || c.is_whitespace())
        .filter(|s| !s.is_empty())
        .map(|w| {
            let mut chars = w.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect();
    if words.is_empty() {
        "PDF Technical Document".to_string()
    } else {
        words.join(" ")
    }
}

/// Tạo một file PDF 1.4 mẫu chứa tài liệu kỹ thuật để chạy thử nghiệm luồng `PDF -> anydoc -> Wiki`.
pub fn build_sample_technical_pdf() -> Vec<u8> {
    let lines: &[(u16, &str)] = &[
        (18, "Hybrid Vector and Lexical Retrieval in Local LLM Wikis"),
        (14, "1. Architecture Overview"),
        (
            11,
            "Modern retrieval systems often suffer from context fragmentation when chunking documents blindly.",
        ),
        (
            11,
            "Combining Andrej Karpathy LLM Wiki architecture with QMD hybrid search solves this problem.",
        ),
        (
            11,
            "Raw documents in sources are converted from PDF to Markdown using Anydoc and indexed locally.",
        ),
        (14, "2. Reciprocal Rank Fusion and SQLite FTS5"),
        (
            11,
            "Lexical search uses SQLite FTS5 with BM25 scoring to capture exact identifiers and symbols.",
        ),
        (
            11,
            "Semantic search uses cosine similarity over dense embeddings stored in chunk vectors.",
        ),
        (
            11,
            "Reciprocal Rank Fusion merges both ranked lists with constant k equal to 60 and a 2x primary weight.",
        ),
        (
            11,
            "Top ranked candidates receive an explicit rank bonus of 0.05 for rank 1 and 0.02 for ranks 2 to 3.",
        ),
        (14, "3. Obsidian Graph View and Bidirectional WikiLinks"),
        (
            11,
            "Each compiled source summary in wiki sources links to canonical Entity and Concept pages.",
        ),
        (
            11,
            "Wiki Lint audits broken links, orphan wiki pages, and uningested sources automatically.",
        ),
    ];

    let mut stream = String::from("BT\n");
    let mut y = 730i32;
    for &(font_size, text) in lines {
        let escaped = text
            .replace('\\', "\\\\")
            .replace('(', "\\(")
            .replace(')', "\\)");
        stream.push_str(&format!(
            "/F1 {font_size} Tf\n1 0 0 1 54 {y} Tm\n({escaped}) Tj\n"
        ));
        y -= if font_size >= 14 { 28 } else { 18 };
    }
    stream.push_str("ET\n");

    let objects = [
        "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>".to_string(),
        format!("<< /Length {} >>\nstream\n{stream}endstream", stream.len()),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_string(),
    ];

    let mut pdf = b"%PDF-1.4\n".to_vec();
    let mut offsets = Vec::new();
    for (index, body) in objects.iter().enumerate() {
        offsets.push(pdf.len());
        pdf.extend_from_slice(format!("{} 0 obj\n{body}\nendobj\n", index + 1).as_bytes());
    }
    let xref = pdf.len();
    pdf.extend_from_slice(
        format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
    );
    for off in &offsets {
        pdf.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
    }
    pdf.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            objects.len() + 1
        )
        .as_bytes(),
    );
    pdf
}
