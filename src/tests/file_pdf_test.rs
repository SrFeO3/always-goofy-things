use super::*;

fn sheet(
    num: usize,
    header: Option<&str>,
    footer: Option<&str>,
    page_number: Option<&str>,
    events: Vec<ContentEvent>,
) -> PageSheet {
    PageSheet {
        num,
        header: header.map(|t| Chrome {
            text: t.to_string(),
            section: None,
        }),
        footer: footer.map(|t| Chrome {
            text: t.to_string(),
            section: None,
        }),
        page_number: page_number.map(|t| Chrome {
            text: t.to_string(),
            section: None,
        }),
        artifacts: Vec::new(),
        image_notes: Vec::new(),
        link_notes: Vec::new(),
        annotation_notes: Vec::new(),
        events,
        headings: Vec::new(),
        list_notes: Vec::new(),
        error: None,
    }
}

fn line(x: f32, y: f32, text: &str) -> Line {
    Line {
        x,
        y,
        height: 12.0,
        text: text.to_string(),
        raw: text.to_string(),
    }
}

#[test]
fn test_parse_folios() {
    assert_eq!(parse_page_number("3"), Some(3));
    assert_eq!(parse_page_number("Page 3 of 42"), Some(3));
    assert_eq!(parse_page_number("3/42"), Some(3));
    assert_eq!(parse_page_number("- 3 -"), Some(3));
    assert_eq!(parse_page_number("iii"), None);
    assert_eq!(parse_page_number(""), None);
}

#[test]
fn test_parse_totals() {
    assert_eq!(parse_total("Page 3 of 42"), Some(42));
    assert_eq!(parse_total("3/42"), Some(42));
    assert_eq!(parse_total("3"), None);
}

#[test]
fn test_collapse_blank_runs() {
    assert_eq!(collapse_blank_lines("a\n\n\n\nb\n"), "a\n\nb\n");
    assert_eq!(collapse_blank_lines("a\nb\n"), "a\nb\n");
}

#[test]
fn test_group_chrome_runs() {
    let sheets = [
        sheet(1, Some("H"), Some("F"), Some("1"), vec![]),
        sheet(2, Some("H"), Some("F"), Some("2"), vec![]),
        sheet(3, Some("H"), None, Some("3"), vec![]),
        sheet(4, None, None, Some("4"), vec![]),
    ];
    let refs: Vec<&PageSheet> = sheets.iter().collect();
    let runs = build_runs(&refs, |s| s.header.clone());
    assert_eq!(runs.len(), 2);
    assert_eq!((runs[0].start, runs[0].end), (1, 3));
    assert_eq!(runs[0].chrome.as_ref().map(|c| c.text.as_str()), Some("H"));
    assert_eq!((runs[1].start, runs[1].end), (4, 4));
    assert!(runs[1].chrome.is_none());
}

#[test]
fn test_format_page_lists() {
    assert_eq!(fmt_pages(&[1, 2, 3, 5, 7, 8]), "p.1-p.3, p.5, p.7-p.8");
    assert_eq!(fmt_pages(&[4]), "p.4");
}

#[test]
fn test_flag_page_number_irregularities() {
    let sheets = [
        sheet(1, None, None, Some("1"), vec![]),
        sheet(2, None, None, Some("3"), vec![]), // not +1
        sheet(3, None, None, Some("3"), vec![]), // repeated value
        sheet(4, None, None, None, vec![]),      // missing
    ];
    let facts = FormatFacts::analyze(&sheets, &[], &DocMeta::default(), None);
    let text = facts.render();
    assert!(
        text.contains("page number '3' appears on p.2-p.3"),
        "{}",
        text
    );
    assert!(text.contains("no page number on p.4"), "{}", text);
    assert!(text.contains("p.1: '1'"), "{}", text);
    assert!(text.contains("p.2: '3'"), "{}", text);
    assert!(!text.contains("expected"), "{}", text);
    assert!(!text.contains("duplicates"), "{}", text);
}

#[test]
fn test_report_clean_sequence() {
    let sheets: Vec<PageSheet> = (1..=4)
        .map(|i| {
            sheet(
                i,
                Some("H"),
                Some("F"),
                Some(&i.to_string()),
                vec![ContentEvent::Paragraph("body".to_string())],
            )
        })
        .collect();
    let facts = FormatFacts::analyze(&sheets, &[], &DocMeta::default(), None);
    let text = facts.render();
    assert!(text.contains("p.1-p.4: '1'..'4'"), "{}", text);
    assert!(text.contains("header: p.1-p.4: 'H'"), "{}", text);
    assert!(text.contains("footer: p.1-p.4: 'F'"), "{}", text);
}

#[test]
fn test_sheet_renders_chrome_labels() {
    let s = sheet(
        3,
        Some("Q Report"),
        Some("Page 3 of 42"),
        Some("3"),
        vec![
            ContentEvent::Heading {
                level: 1,
                text: "2. Results".to_string(),
            },
            ContentEvent::Paragraph("Some body.".to_string()),
        ],
    );
    let text = s.render();
    assert!(text.starts_with("--- page 3 ---\n"));
    assert!(text.contains("[header] Q Report\n"));
    assert!(text.contains("[page-number] 3\n"));
    assert!(text.contains("[footer] Page 3 of 42\n"));
    assert!(text.contains("# 2. Results\n"));
}

#[test]
fn test_list_marker_detection() {
    assert_eq!(list_marker("1. First item"), Some("1.".to_string()));
    assert_eq!(list_marker("2) Second item"), Some("2)".to_string()));
    assert_eq!(list_marker("(a) Alpha"), Some("(a)".to_string()));
    assert_eq!(list_marker("- Dash item"), Some("-".to_string()));
    assert_eq!(list_marker("* Star item"), Some("*".to_string()));
    assert_eq!(list_marker("Hello world"), None);
    assert_eq!(list_marker("1.5 million"), None);
    assert_eq!(list_marker("-5 degrees"), None);
}

#[test]
fn test_strip_marker_keeps_rest() {
    assert_eq!(strip_marker("1. First item", "1."), "First item");
    assert_eq!(strip_marker("**1. First item**", "1."), "First item**");
}

#[test]
fn test_build_events_nested_list_and_paragraph() {
    let lines = [
        line(72.0, 140.0, "1. First item"),
        line(72.0, 128.0, "2. Second item"),
        line(90.0, 116.0, "3. Nested item"),
        line(72.0, 104.0, "Paragraph after list."),
    ];
    let mut notes = Vec::new();
    let events = build_events(&lines, &mut notes);
    assert!(notes.is_empty());
    let rendered = events
        .iter()
        .map(render_event)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(rendered.contains("1. First item"), "{}", rendered);
    assert!(rendered.contains("2. Second item"), "{}", rendered);
    assert!(rendered.contains("  3. Nested item"), "{}", rendered);
    assert!(rendered.contains("Paragraph after list."), "{}", rendered);
}

#[test]
fn test_build_events_item_continuation() {
    let lines = [
        line(72.0, 140.0, "1. First item"),
        line(72.0, 128.0, "wrapped continuation"),
        line(72.0, 116.0, "2. Second item"),
    ];
    let mut notes = Vec::new();
    let events = build_events(&lines, &mut notes);
    let rendered = events
        .iter()
        .map(render_event)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        rendered.contains("1. First item wrapped continuation"),
        "{}",
        rendered
    );
}

#[test]
fn test_build_events_paragraph_split() {
    let lines = [
        line(72.0, 140.0, "First paragraph."),
        line(72.0, 128.0, "Still first paragraph."),
        line(72.0, 108.0, "Second paragraph."),
    ];
    let mut notes = Vec::new();
    let events = build_events(&lines, &mut notes);
    assert_eq!(events.len(), 2);
    assert!(
        matches!(&events[0], ContentEvent::Paragraph(t) if t == "First paragraph. Still first paragraph.")
    );
    assert!(matches!(&events[1], ContentEvent::Paragraph(t) if t == "Second paragraph."));
}

#[test]
fn test_list_sequence_note() {
    let lines = [
        line(72.0, 140.0, "1. One"),
        line(72.0, 128.0, "2. Two"),
        line(72.0, 116.0, "4. Four"),
    ];
    let mut notes = Vec::new();
    let _ = build_events(&lines, &mut notes);
    assert!(notes.iter().any(|n| n == "markers 1, 2, 4"), "{:?}", notes);
}

#[test]
fn test_horizontal_rule_filter() {
    let wide_thin = Rect::new(72.0, 400.0, 468.0, 1.5);
    let narrow = Rect::new(72.0, 400.0, 100.0, 1.5);
    let thick = Rect::new(72.0, 400.0, 468.0, 8.0);
    let table = Rect::new(50.0, 300.0, 500.0, 200.0);
    let chrome = Rect::new(50.0, 700.0, 500.0, 20.0);
    let tables = vec![table];
    let chrome_rects = vec![chrome];

    assert!(is_horizontal_rule(&wide_thin, 612.0, &[], &[]));
    assert!(!is_horizontal_rule(&narrow, 612.0, &[], &[]));
    assert!(!is_horizontal_rule(&thick, 612.0, &[], &[]));
    assert!(!is_horizontal_rule(
        &Rect::new(60.0, 350.0, 468.0, 1.5),
        612.0,
        &tables,
        &[]
    ));
    assert!(!is_horizontal_rule(
        &Rect::new(60.0, 705.0, 468.0, 1.5),
        612.0,
        &[],
        &chrome_rects
    ));
}

#[test]
fn test_render_table_preserves_grid() {
    use pdf_oxide::structure::table_extractor::{TableCell, TableRow};

    let mut table = Table::new();
    table.has_header = true;
    table.col_count = 3;
    let mut header = TableRow::new(true);
    header.add_cell(TableCell::new("Name".to_string(), true));
    header.add_cell(TableCell::new("Qty".to_string(), true));
    header.add_cell(TableCell::new("Price".to_string(), true));
    table.add_row(header);
    let mut body = TableRow::new(false);
    body.add_cell(TableCell::new("Widget|XL".to_string(), false));
    body.add_cell(TableCell::new("2".to_string(), false));
    body.add_cell(TableCell::new("10".to_string(), false));
    table.add_row(body);

    let text = render_table(&table);
    assert!(text.contains("| Name | Qty | Price |"), "{}", text);
    assert!(text.contains("| Widget\\|XL | 2 | 10 |"), "{}", text);
    assert!(text.contains("|---"), "{}", text);
}

#[test]
fn test_styled_text_inline_markers() {
    let span = pdf_oxide::layout::TextSpan {
        text: "term".to_string(),
        font_weight: pdf_oxide::layout::FontWeight::Bold,
        ..Default::default()
    };
    assert_eq!(styled_text(&span), "**term**");
    let mut span = pdf_oxide::layout::TextSpan {
        text: "term".to_string(),
        ..Default::default()
    };
    span.is_italic = true;
    assert_eq!(styled_text(&span), "*term*");
    span.is_italic = false;
    span.is_monospace = true;
    assert_eq!(styled_text(&span), "`term`");
    span.is_monospace = false;
    span.text_rise = 0.5;
    assert_eq!(styled_text(&span), "^term^");
    span.text_rise = -0.5;
    assert_eq!(styled_text(&span), "~term~");
}

#[test]
fn test_heading_jump_note() {
    let mut a = sheet(1, None, None, Some("1"), vec![]);
    a.headings = vec![(1, 1)];
    let mut b = sheet(2, None, None, Some("2"), vec![]);
    b.headings = vec![(2, 3)];
    let facts = FormatFacts::analyze(&[a, b], &[], &DocMeta::default(), None);
    let text = facts.render();
    assert!(text.contains("p.2: H3 follows H1"), "{}", text);
}

#[test]
fn test_extracted_range_fact() {
    let sheets = (1..=8)
        .map(|i| sheet(i, None, None, None, vec![]))
        .collect::<Vec<_>>();
    let sizes: Vec<(usize, u32, u32)> = (1..=8).map(|p| (p, 595, 842)).collect();
    let facts = FormatFacts::analyze(&sheets, &sizes, &DocMeta::default(), Some((3, 7)));
    let text = facts.render();
    // `pages:` is the whole-document count, independent of the range.
    assert!(text.contains("pages: 8"), "{}", text);
    assert!(text.contains("extracted: p.3-p.7"), "{}", text);
    let facts = FormatFacts::analyze(&sheets, &sizes, &DocMeta::default(), Some((3, 3)));
    assert!(facts.render().contains("extracted: p.3"), "{}", text);
}

#[test]
fn test_document_facts_metadata_size_labels() {
    let sheets = (1..=8)
        .map(|i| sheet(i, None, None, None, vec![]))
        .collect::<Vec<_>>();
    let sizes = [
        (1, 595, 842),
        (2, 595, 842),
        (3, 612, 792),
        (4, 612, 792),
        (5, 612, 792),
        (6, 612, 792),
        (7, 612, 792),
        (8, 612, 792),
    ];
    let meta = DocMeta {
        producer: Some("LibreOffice".to_string()),
        title: Some("Spec".to_string()),
        page_labels: vec![PageLabelRange::new(0), PageLabelRange::new(5)],
        ..Default::default()
    };
    let facts = FormatFacts::analyze(&sheets, &sizes, &meta, None);
    let text = facts.render();
    assert!(
        text.contains("metadata: producer 'LibreOffice'"),
        "{}",
        text
    );
    assert!(text.contains("metadata: title 'Spec'"), "{}", text);
    assert!(
        text.contains("page-size: p.1-p.2: '595x842 pt'"),
        "{}",
        text
    );
    assert!(
        text.contains("page-size: p.3-p.8: '612x792 pt'"),
        "{}",
        text
    );
    assert!(text.contains("page-label: p.1-p.5: '1'..'5'"), "{}", text);
    assert!(text.contains("page-label: p.6-p.8: '1'..'3'"), "{}", text);
}

#[test]
fn test_annotation_link_image_notes() {
    assert_eq!(image_note(420, 300), "[image] 420x300");
    assert_eq!(
        link_note("https://example.com"),
        "[link] https://example.com"
    );
    assert_eq!(
        annotation_note("Highlight", None, None),
        "[annotation] Highlight"
    );
    assert_eq!(
        annotation_note("Text", Some("please fix"), Some("kai")),
        "[annotation] Text: 'please fix' (author kai)"
    );
}

#[test]
fn test_render_outline_nested() {
    let outline = vec![OutlineItem {
        title: "Chapter 1".to_string(),
        dest: Some(Destination::PageIndex(0)),
        children: vec![OutlineItem {
            title: "Section 1.1".to_string(),
            dest: None,
            children: vec![],
        }],
    }];
    let text = render_outline(&outline);
    assert!(text.contains("Chapter 1 (p.1)\n"), "{}", text);
    assert!(text.contains("  Section 1.1\n"), "{}", text);
}

#[test]
fn test_render_runs_section_note() {
    let runs = vec![
        Run {
            start: 1,
            end: 2,
            chrome: Some(Chrome {
                text: "H".to_string(),
                section: Some(1),
            }),
        },
        Run {
            start: 3,
            end: 4,
            chrome: Some(Chrome {
                text: "H2".to_string(),
                section: Some(2),
            }),
        },
        Run {
            start: 5,
            end: 6,
            chrome: Some(Chrome {
                text: "H".to_string(),
                section: None,
            }),
        },
        Run {
            start: 7,
            end: 7,
            chrome: None,
        },
    ];
    let text = render_runs("header", &runs);
    assert!(
        text.contains("header: p.1-p.2: 'H' (section 1)"),
        "{}",
        text
    );
    assert!(
        text.contains("header: p.3-p.4: 'H2' (section 2)"),
        "{}",
        text
    );
    assert!(text.contains("header: p.5-p.6: 'H'\n"), "{}", text);
    assert!(text.contains("header: p.7: (none)"), "{}", text);
    assert!(!text.contains("check"), "{}", text);
    assert!(!text.contains("section change"), "{}", text);
}

#[test]
fn test_range_validation() {
    use pdf_oxide::api::Pdf;

    let path = std::env::temp_dir().join(format!("pdf_range_test_{}.pdf", std::process::id()));
    let mut pdf =
        Pdf::from_markdown("# One\n\nBody one.\n\n## Two\n\nBody two.").expect("create pdf");
    pdf.save(&path).expect("save pdf");
    let path_str = path.to_string_lossy().to_string();

    let result = extract_text_from_pdf(&path_str, Some((1, 1))).expect("valid range");
    assert!(result.start == 1 && result.end == 1);
    assert!(result.text.contains("extracted p.1"), "{}", result.text);
    assert!(result.text.contains("extracted: p.1"), "{}", result.text);

    // The `pages:` fact is the whole-document count, even for a range.
    let full = extract_text_from_pdf(&path_str, None).expect("full extract");
    let pages_of = |text: &str| {
        text.lines()
            .find(|l| l.starts_with("pages: "))
            .and_then(|l| l.trim_start_matches("pages: ").parse::<usize>().ok())
            .expect("pages fact")
    };
    assert_eq!(pages_of(&result.text), pages_of(&full.text));

    let err = extract_text_from_pdf(&path_str, Some((3, 2))).unwrap_err();
    assert!(err.contains("Invalid page range"), "{}", err);

    let err = extract_text_from_pdf(&path_str, Some((99, 100))).unwrap_err();
    assert!(err.contains("exceeds"), "{}", err);

    let _ = std::fs::remove_file(&path);
}

#[test]
fn test_smoke_extract_real_pdf() {
    use pdf_oxide::api::Pdf;

    let path = std::env::temp_dir().join(format!("pdf_review_smoke_{}.pdf", std::process::id()));
    let mut pdf =
        Pdf::from_markdown("# Chapter One\n\nHello world.\n\n## Section Two\n\nMore text here.")
            .expect("create pdf");
    pdf.save(&path).expect("save pdf");

    let text = extract_text_from_pdf(&path.to_string_lossy(), None)
        .expect("extract")
        .text;
    let _ = std::fs::remove_file(&path);

    assert!(text.contains("[pdf-review]"), "{}", text);
    assert!(text.contains("[format-facts]"), "{}", text);
    assert!(text.contains("--- page 1 ---"), "{}", text);
    assert!(text.contains("Chapter One"), "{}", text);
    assert!(text.contains("Section Two"), "{}", text);
    assert!(text.contains("Hello world."), "{}", text);
}

#[test]
fn test_save_converted_text_names() {
    let dir = std::env::temp_dir().join(format!("save_conv_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let orig = dir.join("spec.pdf");
    let orig_str = orig.to_string_lossy().to_string();

    // Full extraction keeps the original name.
    let p1 = save_converted_text(&orig_str, "a", None).expect("save");
    assert!(p1.ends_with("spec.pdf_converted_for_llm.txt"), "{}", p1);

    // A page range is embedded so different ranges stay distinct.
    let p2 = save_converted_text(&orig_str, "b", Some((3, 7))).expect("save");
    assert!(
        p2.ends_with("spec.pdf_converted_for_llm_p3-p7.txt"),
        "{}",
        p2
    );

    // A single page uses the same pN-pN form.
    let p3 = save_converted_text(&orig_str, "c", Some((3, 3))).expect("save");
    assert!(
        p3.ends_with("spec.pdf_converted_for_llm_p3-p3.txt"),
        "{}",
        p3
    );

    // Re-saving the same range gets a numeric suffix, as before.
    let p4 = save_converted_text(&orig_str, "d", Some((3, 7))).expect("save");
    assert!(
        p4.ends_with("spec.pdf_converted_for_llm_p3-p7_1.txt"),
        "{}",
        p4
    );

    std::fs::remove_dir_all(&dir).ok();
}
