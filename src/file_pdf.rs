//! PDF extraction for LLM-driven document review (format and content).
//!
//! [`extract_text_from_pdf`] converts a PDF into a review-ready document:
//!
//! - A `[format-facts]` section with factual observations about metadata,
//!   page sizes, page-number sequences, page labels, header/footer content,
//!   heading levels and list markers.
//! - An `[outline]` section with the document bookmarks, when present.
//! - One sheet per page with chrome labeled `[header]` / `[page-number]` /
//!   `[footer]` / `[artifact]` / `[link]` / `[image]` / `[annotation]`,
//!   followed by the body rendered as Markdown: headings (`#` levels),
//!   nested lists, tables, horizontal rules, and inline styles
//!   (`**bold**`, `*italic*`, `^sup^`, `~sub~`).
//!
//! Chrome and structure are kept, not stripped: documents are reviewed for
//! irregularities in headers, footers and page numbers (missing or duplicated
//! folios, headers changing mid-section), so removing them would defeat the
//! purpose.
//!
//! This converter records extraction facts only. It never adds expectations
//! (e.g. "expected"), causes (e.g. "scanned"), or instructions (e.g. "check"):
//! judging the facts is the LLM's job, not this converter's.

use std::collections::{BTreeMap, HashMap};
use std::sync::OnceLock;

use pdf_oxide::annotations::{Annotation, LinkAction};
use pdf_oxide::elements::PathContent;
use pdf_oxide::extractors::images::PdfImage;
use pdf_oxide::extractors::page_labels::{PageLabelExtractor, PageLabelRange};
use pdf_oxide::extractors::xmp::XmpExtractor;
use pdf_oxide::geometry::Rect;
use pdf_oxide::layout::TextSpan;
use pdf_oxide::outline::{Destination, OutlineItem};
use pdf_oxide::structure::table_extractor::Table;
use pdf_oxide::{PdfDocument, RegionRole, StructuredPage};
use regex::Regex;

/// Page count of a PDF (metadata only, no content parsing).
pub fn pdf_page_count(path: &str) -> Result<usize, String> {
    let doc =
        PdfDocument::open(path).map_err(|e| format!("Failed to open PDF '{}': {}", path, e))?;
    doc.page_count().map_err(|e| format!("{}", e))
}

/// Result of an extraction: the review text plus the resolved page range,
/// so callers can echo exact facts without re-parsing the output.
#[derive(Debug)]
pub struct PdfRangeResult {
    pub page_count: usize,
    pub start: usize,
    pub end: usize,
    pub text: String,
}

/// Extract a PDF (or a page range of it) as an LLM-ready review document.
///
/// `range` is a 1-based inclusive physical page range (matching the
/// `--- page N ---` markers); `None` extracts all pages. Invalid ranges
/// return an error. Document-level facts (metadata, page sizes, page
/// labels, outline) cover the whole document; per-page facts cover the
/// extracted range only. Headers, footers and page numbers are preserved
/// as labeled chrome and factually described; body text follows per page
/// with headings, lists, tables and rules rendered as Markdown.
pub fn extract_text_from_pdf(
    path: &str,
    range: Option<(usize, usize)>,
) -> Result<PdfRangeResult, String> {
    let doc =
        PdfDocument::open(path).map_err(|e| format!("Failed to open PDF '{}': {}", path, e))?;

    let page_count = doc.page_count().map_err(|e| format!("{}", e))?;

    let (start, end) = match range {
        None => (1, page_count),
        Some((s, e)) => {
            if s < 1 || e < 1 || s > e {
                return Err(format!(
                    "Invalid page range: p.{}-p.{} (1-based, start must be <= end)",
                    s, e
                ));
            }
            if s > page_count || e > page_count {
                return Err(format!(
                    "Invalid page range: p.{}-p.{} exceeds {} pages",
                    s, e, page_count
                ));
            }
            (s, e)
        }
    };

    // Page sizes for the whole document (media boxes only, no content
    // parsing) so document-level facts stay complete for a range extraction.
    let mut page_sizes: Vec<(usize, u32, u32)> = Vec::with_capacity(page_count);
    for i in 0..page_count {
        let media = doc
            .get_page_media_box(i)
            .unwrap_or((0.0, 0.0, 612.0, 792.0));
        page_sizes.push((i + 1, media.2.round() as u32, media.3.round() as u32));
    }

    let mut sheets = Vec::with_capacity(end - start + 1);
    for i in (start - 1)..end {
        let media = doc
            .get_page_media_box(i)
            .unwrap_or((0.0, 0.0, 612.0, 792.0));
        let page = doc.extract_structured(i).map_err(|e| e.to_string());
        let tables = doc.extract_tables(i).unwrap_or_default();
        let lines = doc.extract_lines(i).unwrap_or_default();
        let images = doc.extract_images(i).unwrap_or_default();
        let annotations = doc.get_annotations(i).unwrap_or_default();
        sheets.push(PageSheet::build(
            i + 1,
            page,
            tables,
            lines,
            media.2,
            images,
            annotations,
        ));
    }

    let xmp = XmpExtractor::extract(&doc).ok().flatten();
    let meta = DocMeta {
        producer: doc.document_producer(),
        creator: doc.document_creator(),
        title: xmp.as_ref().and_then(|x| x.dc_title.clone()),
        authors: xmp.map(|x| x.dc_creator).unwrap_or_default(),
        page_labels: PageLabelExtractor::extract(&doc).unwrap_or_default(),
        outline: doc.get_outline().ok().flatten(),
    };

    let facts = FormatFacts::analyze(&sheets, &page_sizes, &meta, range);

    let legend = match range {
        Some((s, e)) if s == e => format!(
            "[pdf-review] {} ({} pages, extracted p.{}) - format facts and [outline] below; per-page sheets with labeled [header]/[footer]/[page-number]/[artifact]/[link]/[image]/[annotation]; inline **bold** *italic* ^sup^ ~sub~; body text per page.\n",
            path, page_count, s
        ),
        Some((s, e)) => format!(
            "[pdf-review] {} ({} pages, extracted p.{}-p.{}) - format facts and [outline] below; per-page sheets with labeled [header]/[footer]/[page-number]/[artifact]/[link]/[image]/[annotation]; inline **bold** *italic* ^sup^ ~sub~; body text per page.\n",
            path, page_count, s, e
        ),
        None => format!(
            "[pdf-review] {} ({} pages) - format facts and [outline] below; per-page sheets with labeled [header]/[footer]/[page-number]/[artifact]/[link]/[image]/[annotation]; inline **bold** *italic* ^sup^ ~sub~; body text per page.\n",
            path, page_count
        ),
    };

    let mut out = String::new();
    out.push_str(&legend);
    out.push_str(&facts.render());
    if let Some(outline) = &facts.outline {
        out.push_str("[outline]\n");
        out.push_str(&render_outline(outline));
    }
    for sheet in &sheets {
        out.push_str(&sheet.render());
    }
    Ok(PdfRangeResult {
        page_count,
        start,
        end,
        text: out,
    })
}

/// Normalized chrome text (header / footer / page-number folio) plus the
/// tagged-PDF section it belongs to, when known.
#[derive(Clone, PartialEq, Eq)]
struct Chrome {
    text: String,
    section: Option<usize>,
}

/// A content block of the page body, rendered as Markdown.
enum ContentEvent {
    Heading { level: u8, text: String },
    Paragraph(String),
    ListBlock(Vec<ListItem>),
    Table(String),
    Rule,
}

/// A single list item, rendered as `marker + text` at `level` indentation.
struct ListItem {
    x: f32,
    level: usize,
    marker: String,
    text: String,
}

/// One page of the review document.
struct PageSheet {
    /// 1-based page index in document order.
    num: usize,
    header: Option<Chrome>,
    footer: Option<Chrome>,
    page_number: Option<Chrome>,
    artifacts: Vec<String>,
    /// Rendered `[image] WxH` lines, in content-stream order.
    image_notes: Vec<String>,
    /// Rendered `[link] URL` lines, in annotation order.
    link_notes: Vec<String>,
    /// Rendered `[annotation] subtype: contents` lines, in annotation order.
    annotation_notes: Vec<String>,
    /// Content blocks in reading order.
    events: Vec<ContentEvent>,
    /// Heading levels found on this page, in reading order.
    headings: Vec<(usize, u8)>,
    /// Numbered-list marker sequences found on this page (facts only).
    list_notes: Vec<String>,
    error: Option<String>,
}

impl PageSheet {
    fn build(
        num: usize,
        page: Result<StructuredPage, String>,
        tables: Vec<Table>,
        lines: Vec<PathContent>,
        page_width: f32,
        images: Vec<PdfImage>,
        annotations: Vec<Annotation>,
    ) -> Self {
        let mut sheet = Self {
            num,
            header: None,
            footer: None,
            page_number: None,
            artifacts: Vec::new(),
            image_notes: Vec::new(),
            link_notes: Vec::new(),
            annotation_notes: Vec::new(),
            events: Vec::new(),
            headings: Vec::new(),
            list_notes: Vec::new(),
            error: None,
        };
        let page = match page {
            Ok(page) => page,
            Err(e) => {
                sheet.error = Some(e);
                return sheet;
            }
        };

        for img in &images {
            sheet
                .image_notes
                .push(image_note(img.width(), img.height()));
        }
        for a in &annotations {
            match &a.action {
                Some(LinkAction::Uri(url)) => sheet.link_notes.push(link_note(url)),
                _ => {
                    if a.contents.is_some() || a.author.is_some() {
                        sheet.annotation_notes.push(annotation_note(
                            a.subtype.as_deref().unwrap_or("Annot"),
                            a.contents.as_deref(),
                            a.author.as_deref(),
                        ));
                    }
                }
            }
        }

        // Pass 1: chrome and chrome bounding boxes (used to exclude rules).
        let mut chrome_bboxes: Vec<Rect> = Vec::new();
        for region in &page.regions {
            let text = region.text.trim();
            if text.is_empty() {
                continue;
            }
            match &region.kind {
                RegionRole::Header => {
                    chrome_bboxes.push(region.bbox);
                    push_chrome(&mut sheet.header, text, region.section_id);
                }
                RegionRole::Footer => {
                    chrome_bboxes.push(region.bbox);
                    push_chrome(&mut sheet.footer, text, region.section_id);
                }
                RegionRole::PageNumber => {
                    chrome_bboxes.push(region.bbox);
                    push_chrome(&mut sheet.page_number, text, region.section_id);
                }
                RegionRole::Artifact => sheet.artifacts.push(normalize(text)),
                _ => {}
            }
        }

        // Tables become Markdown pipe tables; their spans are excluded from
        // body rendering via bounding-box intersection.
        let table_bboxes: Vec<Rect> = tables.iter().filter_map(|t| t.bbox).collect();
        let mut extras: Vec<(f32, ContentEvent)> = tables
            .iter()
            .filter(|t| !t.rows.is_empty() && t.col_count > 0)
            .map(|t| {
                let y = t.bbox.map(|b| b.y + b.height / 2.0).unwrap_or(f32::MAX);
                (y, ContentEvent::Table(render_table(t)))
            })
            .collect();

        // Full-width horizontal rules render as `---`, positioned by y.
        extras.extend(
            find_rules(&lines, page_width, &table_bboxes, &chrome_bboxes)
                .into_iter()
                .map(|y| (y, ContentEvent::Rule)),
        );
        extras.sort_by(|a, b| a.0.total_cmp(&b.0));

        // Pass 2: content in reading order, with tables and rules merged in
        // by vertical position.
        let mut extra_i = 0usize;
        for region in &page.regions {
            let text = region.text.trim();
            if text.is_empty() {
                continue;
            }
            match &region.kind {
                RegionRole::Header
                | RegionRole::Footer
                | RegionRole::PageNumber
                | RegionRole::Artifact => {}
                RegionRole::StructuralHeading { level } => {
                    while extra_i < extras.len() && extras[extra_i].0 <= region.bbox.y {
                        sheet.events.push(std::mem::replace(
                            &mut extras[extra_i].1,
                            ContentEvent::Rule,
                        ));
                        extra_i += 1;
                    }
                    let level = (*level as usize).clamp(1, 6) as u8;
                    sheet.headings.push((num, level));
                    sheet.events.push(ContentEvent::Heading {
                        level,
                        text: normalize(text),
                    });
                }
                RegionRole::MarginalLabel => {
                    sheet.events.push(ContentEvent::Paragraph(normalize(text)));
                }
                RegionRole::BodyBlock => {
                    let lines = assemble_lines(&region.spans, &table_bboxes);
                    let mut notes = Vec::new();
                    sheet.events.extend(build_events(&lines, &mut notes));
                    sheet.list_notes.extend(notes);
                }
            }
        }
        while extra_i < extras.len() {
            sheet.events.push(std::mem::replace(
                &mut extras[extra_i].1,
                ContentEvent::Rule,
            ));
            extra_i += 1;
        }
        sheet
    }

    fn render(&self) -> String {
        let mut out = format!("--- page {} ---\n", self.num);
        if let Some(e) = &self.error {
            out.push_str(&format!("[Text extraction error: {}]\n", e));
            return out;
        }
        if let Some(c) = &self.header {
            out.push_str(&format!("[header] {}\n", c.text));
        }
        if let Some(c) = &self.page_number {
            out.push_str(&format!("[page-number] {}\n", c.text));
        }
        if let Some(c) = &self.footer {
            out.push_str(&format!("[footer] {}\n", c.text));
        }
        for a in &self.artifacts {
            out.push_str(&format!("[artifact] {}\n", a));
        }
        for n in &self.image_notes {
            out.push_str(n);
            out.push('\n');
        }
        for n in &self.link_notes {
            out.push_str(n);
            out.push('\n');
        }
        for n in &self.annotation_notes {
            out.push_str(n);
            out.push('\n');
        }
        if !self.events.is_empty() {
            out.push('\n');
            for event in &self.events {
                out.push_str(&render_event(event));
                out.push('\n');
            }
        }
        collapse_blank_lines(&out)
    }
}

/// Merge an additional region into chrome, joining multi-part chrome with ` | `.
fn push_chrome(target: &mut Option<Chrome>, text: &str, section: Option<usize>) {
    let text = normalize(text);
    if text.is_empty() {
        return;
    }
    match target {
        Some(c) => {
            c.text.push_str(" | ");
            c.text.push_str(&text);
        }
        None => *target = Some(Chrome { text, section }),
    }
}

/// Collapse whitespace runs to single spaces (single-line chrome text).
fn normalize(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Rendered image fact: `[image] WxH` in pixels.
fn image_note(width: u32, height: u32) -> String {
    format!("[image] {}x{}", width, height)
}

/// Rendered hyperlink fact: `[link] URL`.
fn link_note(url: &str) -> String {
    format!("[link] {}", url)
}

/// Rendered annotation fact: `[annotation] subtype: contents (author X)`.
fn annotation_note(subtype: &str, contents: Option<&str>, author: Option<&str>) -> String {
    let mut out = format!("[annotation] {}", subtype);
    if let Some(c) = contents {
        out.push_str(&format!(": '{}'", c));
    }
    if let Some(a) = author {
        out.push_str(&format!(" (author {})", a));
    }
    out
}

/// Render the document outline (bookmarks) as a nested list, with the target
/// page when the destination is a direct page reference.
fn render_outline(items: &[OutlineItem]) -> String {
    let mut out = String::new();
    render_outline_level(items, 0, &mut out);
    out
}

fn render_outline_level(items: &[OutlineItem], depth: usize, out: &mut String) {
    for item in items {
        let dest = match &item.dest {
            Some(Destination::PageIndex(idx)) => format!(" (p.{})", idx + 1),
            _ => String::new(),
        };
        out.push_str(&format!(
            "{}{}{}\n",
            "  ".repeat(depth),
            normalize(&item.title),
            dest
        ));
        render_outline_level(&item.children, depth + 1, out);
    }
}

/// Collapse runs of blank lines to a single blank line.
fn collapse_blank_lines(s: &str) -> String {
    let mut out = String::new();
    let mut blanks = 0usize;
    for line in s.lines() {
        if line.trim().is_empty() {
            blanks += 1;
            if blanks > 1 {
                continue;
            }
            out.push('\n');
        } else {
            blanks = 0;
            out.push_str(line);
            out.push('\n');
        }
    }
    while out.ends_with('\n') {
        out.pop();
    }
    out.push('\n');
    out
}

/// Parse the first integer in a folio, e.g. `"3"`, `"Page 3 of 42"`, `"3/42"`.
fn parse_page_number(text: &str) -> Option<u32> {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\d+").unwrap())
        .find(text)
        .and_then(|m| m.as_str().parse().ok())
}

/// Parse the total in `"x of y"` / `"x/y"` folios.
fn parse_total(text: &str) -> Option<u32> {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?:\d+\s*(?:/|of)\s*)(\d+)").unwrap())
        .captures(text)
        .and_then(|c| c.get(1))
        .and_then(|m| m.as_str().parse().ok())
}

// --- body assembly ----------------------------------------------------------

/// A single text line assembled from spans, with position for layout analysis.
struct Line {
    x: f32,
    y: f32,
    height: f32,
    /// Styled text (`**bold**`, `*italic*`, ...).
    text: String,
    /// Unstyled text, used for list-marker detection.
    raw: String,
}

/// Group spans into lines and apply inline styling. Spans that overlap a
/// table bounding box are dropped (the table is rendered separately).
fn assemble_lines(spans: &[TextSpan], table_bboxes: &[Rect]) -> Vec<Line> {
    let mut buckets: BTreeMap<i64, Vec<&TextSpan>> = BTreeMap::new();
    for span in spans {
        if span.text.trim().is_empty() {
            continue;
        }
        if table_bboxes.iter().any(|t| t.intersects(&span.bbox)) {
            continue;
        }
        let key = (span.bbox.y / 3.0).round() as i64;
        buckets.entry(key).or_default().push(span);
    }
    let mut lines = Vec::new();
    for (_key, mut bucket) in buckets.into_iter().rev() {
        bucket.sort_by(|a, b| a.bbox.x.total_cmp(&b.bbox.x));
        let text = bucket
            .iter()
            .map(|s| styled_text(s))
            .collect::<Vec<_>>()
            .join(" ");
        let raw = bucket
            .iter()
            .map(|s| s.text.trim())
            .collect::<Vec<_>>()
            .join(" ");
        let text = normalize(&text);
        if text.is_empty() {
            continue;
        }
        let x = bucket
            .iter()
            .map(|s| s.bbox.x)
            .fold(f32::INFINITY, f32::min);
        let y = bucket[0].bbox.y;
        let height = bucket.iter().map(|s| s.bbox.height).fold(0.0f32, f32::max);
        lines.push(Line {
            x,
            y,
            height,
            text,
            raw,
        });
    }
    lines
}

/// Wrap a span's text with inline Markdown style markers.
fn styled_text(span: &TextSpan) -> String {
    let text = span.text.trim();
    if text.is_empty() {
        return String::new();
    }
    if span.text_rise > 0.15 {
        return format!("^{}^", text);
    }
    if span.text_rise < -0.15 {
        return format!("~{}~", text);
    }
    let bold = span.font_weight.is_bold();
    let italic = span.is_italic;
    let mono = span.is_monospace;
    let mut out = String::new();
    if bold {
        out.push_str("**");
    }
    if italic {
        out.push('*');
    }
    if mono {
        out.push('`');
    }
    out.push_str(text);
    if mono {
        out.push('`');
    }
    if italic {
        out.push('*');
    }
    if bold {
        out.push_str("**");
    }
    out
}

/// Detect a list marker at the start of a line, e.g. `1.`, `-`, `(a)`, `*`.
fn list_marker(text: &str) -> Option<String> {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^([•◦‣▪・*\-]|\d{1,3}[.)]|[a-zA-Z][.)]|\([a-zA-Z0-9]+\))(?:\s+|$)").unwrap()
    })
    .captures(text)
    .map(|c| c.get(1).unwrap().as_str().to_string())
}

/// Strip the leading marker from a styled line, keeping the styling of the
/// remaining text. Falls back to the styled text unchanged.
fn strip_marker(styled: &str, marker: &str) -> String {
    match styled.find(marker) {
        Some(idx) => {
            let rest = styled[idx + marker.len()..].trim_start().to_string();
            if rest.is_empty() {
                styled.trim().to_string()
            } else {
                rest
            }
        }
        None => styled.trim().to_string(),
    }
}

/// Build content blocks from lines: paragraphs (split by vertical gaps) and
/// list blocks (items with markers, nested by indentation).
fn build_events(lines: &[Line], list_notes: &mut Vec<String>) -> Vec<ContentEvent> {
    let mut events: Vec<ContentEvent> = Vec::new();
    let mut items: Vec<ListItem> = Vec::new();
    let mut para: Vec<String> = Vec::new();
    let mut prev_y = 0.0f32;
    let mut prev_h = 0.0f32;
    let mut first = true;

    for line in lines {
        // PDF y grows upward, so reading order descends; the gap between a
        // line and the one above it is (prev_top - prev_height) - this_top.
        let gap = if first {
            first = false;
            f32::INFINITY
        } else {
            (prev_y - prev_h) - line.y
        };
        if let Some(marker) = list_marker(&line.raw) {
            flush_paragraph(&mut events, &mut para);
            items.push(ListItem {
                x: line.x,
                level: 0,
                text: strip_marker(&line.text, &marker),
                marker,
            });
            prev_y = line.y;
            prev_h = line.height;
            continue;
        }
        if let Some(last) = items.last_mut() {
            let contiguous = gap <= 0.8 * line.height;
            if contiguous && line.x >= last.x - 1.0 {
                // Continuation of the previous item (wrapped line).
                last.text.push(' ');
                last.text.push_str(&line.text);
                prev_y = line.y;
                prev_h = line.height;
                continue;
            }
            flush_list(&mut events, &mut items, list_notes);
        }
        if !para.is_empty() && gap > 0.5 * line.height {
            flush_paragraph(&mut events, &mut para);
        }
        para.push(line.text.clone());
        prev_y = line.y;
        prev_h = line.height;
    }
    flush_paragraph(&mut events, &mut para);
    flush_list(&mut events, &mut items, list_notes);
    events
}

fn flush_paragraph(events: &mut Vec<ContentEvent>, para: &mut Vec<String>) {
    if para.is_empty() {
        return;
    }
    events.push(ContentEvent::Paragraph(para.join(" ")));
    para.clear();
}

fn flush_list(
    events: &mut Vec<ContentEvent>,
    items: &mut Vec<ListItem>,
    list_notes: &mut Vec<String>,
) {
    if items.is_empty() {
        return;
    }
    // Nesting level = index of this item's indent among the distinct indents.
    let mut indents: Vec<f32> = items.iter().map(|i| i.x).collect();
    indents.sort_by(|a, b| a.total_cmp(b));
    indents.dedup();
    for item in items.iter_mut() {
        let idx = indents
            .iter()
            .position(|x| (*x - item.x).abs() < 0.5)
            .unwrap_or(0);
        item.level = idx.min(5);
    }
    let block = std::mem::take(items);
    check_list_sequence(&block, list_notes);
    events.push(ContentEvent::ListBlock(block));
}

/// Flag top-level numbered lists whose markers do not increment by 1.
/// Records the marker values only, as facts.
fn check_list_sequence(block: &[ListItem], list_notes: &mut Vec<String>) {
    let nums: Vec<u32> = block
        .iter()
        .filter(|i| i.level == 0)
        .filter_map(|i| i.marker.trim_end_matches(['.', ')']).parse().ok())
        .collect();
    if nums.len() < 2 {
        return;
    }
    for pair in nums.windows(2) {
        if pair[1] != pair[0] + 1 {
            let shown = nums
                .iter()
                .map(|n| n.to_string())
                .collect::<Vec<_>>()
                .join(", ");
            list_notes.push(format!("markers {}", shown));
            return;
        }
    }
}

fn render_event(event: &ContentEvent) -> String {
    match event {
        ContentEvent::Heading { level, text } => {
            format!("{} {}", "#".repeat(*level as usize), text)
        }
        ContentEvent::Paragraph(text) => text.clone(),
        ContentEvent::ListBlock(items) => items
            .iter()
            .map(|i| format!("{}{} {}", "  ".repeat(i.level), i.marker, i.text))
            .collect::<Vec<_>>()
            .join("\n"),
        ContentEvent::Table(text) => text.clone(),
        ContentEvent::Rule => "---".to_string(),
    }
}

// --- tables ----------------------------------------------------------------

/// Render a table as a Markdown pipe table, preserving cell structure.
fn render_table(table: &Table) -> String {
    let cols = table.col_count.max(1);
    let mut out = String::new();
    for (ri, row) in table.rows.iter().enumerate() {
        if ri == 1 && table.has_header {
            out.push('|');
            for _ in 0..cols {
                out.push_str("---|");
            }
            out.push('\n');
        }
        out.push('|');
        for ci in 0..cols {
            let mut text = match row.cells.get(ci) {
                Some(cell) => {
                    let mut t = cell.text.replace('|', "\\|").replace('\n', " ");
                    t = normalize(&t);
                    if cell.colspan > 1 {
                        t = format!("{} (colspan {})", t, cell.colspan);
                    }
                    if cell.rowspan > 1 {
                        t = format!("{} (rowspan {})", t, cell.rowspan);
                    }
                    t
                }
                None => String::new(),
            };
            text = text.trim().to_string();
            out.push(' ');
            out.push_str(&text);
            out.push_str(" |");
        }
        out.push('\n');
    }
    out
}

// --- horizontal rules ------------------------------------------------------

/// Detect full-width horizontal rules (rendered as `---`): thin, wide, and
/// not part of a table border or the page chrome.
fn find_rules(
    lines: &[PathContent],
    page_width: f32,
    tables: &[Rect],
    chrome: &[Rect],
) -> Vec<f32> {
    lines
        .iter()
        .filter(|l| is_horizontal_rule(&l.bbox, page_width, tables, chrome))
        .map(|l| l.bbox.y + l.bbox.height / 2.0)
        .collect()
}

fn is_horizontal_rule(bbox: &Rect, page_width: f32, tables: &[Rect], chrome: &[Rect]) -> bool {
    if bbox.height > 3.0 {
        return false;
    }
    if bbox.width < page_width * 0.5 {
        return false;
    }
    if tables.iter().any(|t| t.intersects(bbox)) {
        return false;
    }
    if chrome.iter().any(|c| c.intersects(bbox)) {
        return false;
    }
    true
}

// --- format facts ---------------------------------------------------------

/// A contiguous run of pages sharing the same chrome (or lacking it).
struct Run {
    start: usize,
    end: usize,
    chrome: Option<Chrome>,
}

fn build_runs(sheets: &[&PageSheet], pick: impl Fn(&PageSheet) -> Option<Chrome>) -> Vec<Run> {
    let mut runs: Vec<Run> = Vec::new();
    for sheet in sheets {
        let chrome = pick(sheet);
        if let Some(last) = runs.last_mut().filter(|last| last.chrome == chrome) {
            last.end = sheet.num;
            continue;
        }
        runs.push(Run {
            start: sheet.num,
            end: sheet.num,
            chrome,
        });
    }
    runs
}

/// Document-level facts collected once per PDF: metadata, page labels,
/// bookmarks, and per-page sizes in points.
#[derive(Default)]
struct DocMeta {
    producer: Option<String>,
    creator: Option<String>,
    title: Option<String>,
    authors: Vec<String>,
    page_labels: Vec<PageLabelRange>,
    outline: Option<Vec<OutlineItem>>,
}

/// Factual observations about document format, computed from structured
/// extraction. Records facts only: no expectations, causes, or instructions.
struct FormatFacts {
    page_count: usize,
    /// 1-based extracted page range, when a range was requested.
    extracted: Option<(usize, usize)>,
    metadata_notes: Vec<String>,
    size_notes: Vec<String>,
    page_number_notes: Vec<String>,
    label_notes: Vec<String>,
    header_runs: Vec<Run>,
    footer_runs: Vec<Run>,
    heading_notes: Vec<String>,
    list_notes: Vec<String>,
    notes: Vec<String>,
    outline: Option<Vec<OutlineItem>>,
}

impl FormatFacts {
    fn analyze(
        sheets: &[PageSheet],
        page_sizes: &[(usize, u32, u32)],
        meta: &DocMeta,
        range: Option<(usize, usize)>,
    ) -> Self {
        // `pages:` is the whole-document count, even for a range extraction;
        // `page_sizes` always covers every page of the document.
        let page_count = page_sizes.len();
        let ok: Vec<&PageSheet> = sheets.iter().filter(|s| s.error.is_none()).collect();

        let mut mn: Vec<String> = Vec::new();
        if let Some(p) = &meta.producer {
            mn.push(format!("producer '{}'", p));
        }
        if let Some(c) = &meta.creator {
            mn.push(format!("creator '{}'", c));
        }
        if let Some(t) = &meta.title {
            mn.push(format!("title '{}'", t));
        }
        if !meta.authors.is_empty() {
            mn.push(format!("author '{}'", meta.authors.join(", ")));
        }

        // Page sizes in points, grouped into runs of identical dimensions.
        let mut size_notes: Vec<String> = Vec::new();
        let mut size_runs: Vec<(usize, usize, u32, u32)> = Vec::new();
        for (page, w, h) in page_sizes {
            if let Some((_, end, lw, lh)) = size_runs.last_mut()
                && *page == *end + 1
                && *lw == *w
                && *lh == *h
            {
                *end = *page;
                continue;
            }
            size_runs.push((*page, *page, *w, *h));
        }
        for (start, end, w, h) in size_runs {
            let range = if start == end {
                format!("p.{}", start)
            } else {
                format!("p.{}-p.{}", start, end)
            };
            size_notes.push(format!("{}: '{}x{} pt'", range, w, h));
        }

        // Page labels (/PageLabels): the formatted label of each range.
        let mut label_notes: Vec<String> = Vec::new();
        for (i, range) in meta.page_labels.iter().enumerate() {
            let start = range.start_page;
            let end = meta
                .page_labels
                .get(i + 1)
                .map(|next| next.start_page.saturating_sub(1))
                .unwrap_or(page_count.saturating_sub(1))
                .max(start);
            let first = range.format_label(start);
            let last = range.format_label(end);
            if start == end {
                label_notes.push(format!("p.{}: '{}'", start + 1, first));
            } else {
                label_notes.push(format!(
                    "p.{}-p.{}: '{}'..'{}'",
                    start + 1,
                    end + 1,
                    first,
                    last
                ));
            }
        }

        let mut pn: Vec<String> = Vec::new();
        let mut notes: Vec<String> = Vec::new();

        // --- page numbers ---
        let mut entries: Vec<(usize, u32)> = Vec::new();
        for s in &ok {
            if let Some(c) = &s.page_number {
                match parse_page_number(&c.text) {
                    Some(n) => entries.push((s.num, n)),
                    None => pn.push(format!(
                        "p.{}: page number '{}' is not numeric",
                        s.num, c.text
                    )),
                }
            }
        }

        let missing: Vec<usize> = ok
            .iter()
            .filter(|s| s.page_number.is_none())
            .map(|s| s.num)
            .collect();
        if missing.len() == ok.len() && !ok.is_empty() {
            pn.push("no page numbers on any page".to_string());
        } else if !missing.is_empty() {
            pn.push(format!("no page number on {}", fmt_pages(&missing)));
        }

        // Consecutive pages whose folios increment by 1 form a run; every
        // other page is listed individually. Facts only, no expectations.
        let mut runs: Vec<(usize, usize, u32, u32)> = Vec::new();
        for (page, n) in &entries {
            if let Some((_, end, _, last)) = runs.last_mut()
                && *page == *end + 1
                && *n == *last + 1
            {
                *end = *page;
                *last = *n;
                continue;
            }
            runs.push((*page, *page, *n, *n));
        }
        for (start, end, first, last) in runs {
            if start == end {
                pn.push(format!("p.{}: '{}'", start, first));
            } else {
                pn.push(format!("p.{}-p.{}: '{}'..'{}'", start, end, first, last));
            }
        }

        // Repeated folio values, as facts: page number '3' appears on p.2-p.3.
        let mut seen: HashMap<u32, Vec<usize>> = HashMap::new();
        for (page, n) in &entries {
            seen.entry(*n).or_default().push(*page);
        }
        let mut dup_values: Vec<u32> = seen
            .iter()
            .filter(|(_, pages)| pages.len() > 1)
            .map(|(v, _)| *v)
            .collect();
        dup_values.sort();
        for v in dup_values {
            pn.push(format!(
                "page number '{}' appears on {}",
                v,
                fmt_pages(&seen[&v])
            ));
        }

        // "x of y" totals, as facts: 'of 42' on p.2-p.42, or per-page values.
        let totals: Vec<(usize, u32)> = ok
            .iter()
            .filter_map(|s| {
                let c = s.page_number.as_ref()?;
                parse_total(&c.text).map(|t| (s.num, t))
            })
            .collect();
        if !totals.is_empty() {
            let mut by_total: BTreeMap<u32, Vec<usize>> = BTreeMap::new();
            for (page, total) in &totals {
                by_total.entry(*total).or_default().push(*page);
            }
            if by_total.len() == 1 {
                let (total, pages) = by_total.iter().next().unwrap();
                pn.push(format!("'of {}' on {}", total, fmt_pages(pages)));
            } else {
                pn.push(format!(
                    "'of' totals: {}",
                    totals
                        .iter()
                        .map(|(p, t)| format!("p.{} '{}'", p, t))
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
        }

        // --- headers / footers ---
        let header_runs = build_runs(&ok, |s| s.header.clone());
        let footer_runs = build_runs(&ok, |s| s.footer.clone());

        // --- headings: record level changes of more than one, as facts ---
        let mut heading_notes: Vec<String> = Vec::new();
        let mut prev_heading: Option<(usize, u8)> = None;
        for s in &ok {
            for (page, level) in &s.headings {
                if let Some((_, prev_level)) = prev_heading
                    && *level > prev_level + 1
                {
                    heading_notes.push(format!("p.{}: H{} follows H{}", page, level, prev_level));
                }
                prev_heading = Some((*page, *level));
            }
        }

        // --- numbered-list irregularities ---
        let list_notes: Vec<String> = ok
            .iter()
            .flat_map(|s| s.list_notes.iter().map(|n| format!("p.{}: {}", s.num, n)))
            .collect();

        // --- misc ---
        for s in sheets {
            if s.error.is_some() {
                notes.push(format!("p.{}: extraction failed", s.num));
            }
        }
        let no_body: Vec<usize> = ok
            .iter()
            .filter(|s| s.events.is_empty())
            .map(|s| s.num)
            .collect();
        if no_body.len() == ok.len() && !ok.is_empty() {
            notes.push("no body text on any page".to_string());
        } else if !no_body.is_empty() {
            notes.push(format!("no body text on {}", fmt_pages(&no_body)));
        }

        Self {
            page_count,
            extracted: range,
            metadata_notes: mn,
            size_notes,
            page_number_notes: pn,
            label_notes,
            header_runs,
            footer_runs,
            heading_notes,
            list_notes,
            notes,
            outline: meta.outline.clone(),
        }
    }

    fn render(&self) -> String {
        let mut out = String::from("[format-facts]\n");
        out.push_str(&format!("pages: {}\n", self.page_count));
        if let Some((s, e)) = self.extracted {
            if s == e {
                out.push_str(&format!("extracted: p.{}\n", s));
            } else {
                out.push_str(&format!("extracted: p.{}-p.{}\n", s, e));
            }
        }
        for n in &self.metadata_notes {
            out.push_str(&format!("metadata: {}\n", n));
        }
        for n in &self.size_notes {
            out.push_str(&format!("page-size: {}\n", n));
        }
        for n in &self.page_number_notes {
            out.push_str(&format!("page-number: {}\n", n));
        }
        for n in &self.label_notes {
            out.push_str(&format!("page-label: {}\n", n));
        }
        out.push_str(&render_runs("header", &self.header_runs));
        out.push_str(&render_runs("footer", &self.footer_runs));
        for n in &self.heading_notes {
            out.push_str(&format!("heading: {}\n", n));
        }
        for n in &self.list_notes {
            out.push_str(&format!("list: {}\n", n));
        }
        for n in &self.notes {
            out.push_str(&format!("note: {}\n", n));
        }
        out
    }
}

/// Render chrome runs with the tagged-PDF section each run belongs to,
/// when known. Facts only: no expectations about what should change.
fn render_runs(label: &str, runs: &[Run]) -> String {
    let mut out = String::new();
    for run in runs {
        let range = if run.start == run.end {
            format!("p.{}", run.start)
        } else {
            format!("p.{}-p.{}", run.start, run.end)
        };
        match &run.chrome {
            Some(c) => {
                let section = c
                    .section
                    .map(|s| format!(" (section {})", s))
                    .unwrap_or_default();
                out.push_str(&format!("{}: {}: '{}'{}\n", label, range, c.text, section));
            }
            None => out.push_str(&format!("{}: {}: (none)\n", label, range)),
        }
    }
    out
}

/// Format page lists with ranges, e.g. `p.1-p.3, p.5, p.7-p.8`.
fn fmt_pages(pages: &[usize]) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut iter = pages.iter().copied();
    let Some(mut start) = iter.next() else {
        return String::new();
    };
    let mut prev = start;
    for p in iter {
        if p == prev + 1 {
            prev = p;
            continue;
        }
        parts.push(range_str(start, prev));
        start = p;
        prev = p;
    }
    parts.push(range_str(start, prev));
    parts.join(", ")
}

fn range_str(start: usize, end: usize) -> String {
    if start == end {
        format!("p.{}", start)
    } else {
        format!("p.{}-p.{}", start, end)
    }
}

/// Save extracted PDF text alongside the original file.
///
/// The output name embeds the requested page range so different ranges of
/// the same file stay distinct: `{file}_converted_for_llm.txt` (full) or
/// `{file}_converted_for_llm_p3-p7.txt` (pages 3-7; a single page is
/// `_p3-p3`). If the name already exists, a numeric suffix is appended
/// (`_1`, `_2`, ...) as before.
pub fn save_converted_text(
    orig_path: &str,
    text: &str,
    page_range: Option<(usize, usize)>,
) -> Result<String, String> {
    use std::path::Path;

    let range_tag = match page_range {
        Some((start, end)) => format!("_p{}-p{}", start, end),
        None => String::new(),
    };
    let base = format!("{}_converted_for_llm{}.txt", orig_path, range_tag);

    let path = if Path::new(&base).exists() {
        let stem = format!("{}_converted_for_llm{}_", orig_path, range_tag);
        (1..)
            .map(|n| format!("{}{}.txt", stem, n))
            .find(|p| !Path::new(p).exists())
            .unwrap()
    } else {
        base
    };

    std::fs::write(&path, text).map_err(|e| format!("Failed to write '{}': {}", path, e))?;

    let size_str = crate::attach::format_file_size(text.len() as u64);
    println!(
        "{}[Converted] {} (Markdown, {}){}",
        crate::startup::C_DIM_GRAY,
        path,
        size_str,
        crate::startup::RESET
    );

    Ok(path)
}

#[cfg(test)]
#[path = "tests/file_pdf_test.rs"]
mod tests;
