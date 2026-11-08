//! The native HTML renderer against real-world email shapes (`tests/fixtures/html`, sources in
//! `LICENSES.md`): what the layout tree keeps from each, as the reader would draw it.

use envoy_core::html::{self, Align, BoxStyle, Doc, Family, Issue, Len, LineHeight, Node, Para, Verdict};

fn load(name: &str) -> Doc {
    let path = format!("{}/tests/fixtures/html/{name}.html", env!("CARGO_MANIFEST_DIR"));
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
    html::parse(&text)
}

/// Every node, depth first.
fn all(nodes: &[Node]) -> Vec<&Node> {
    fn walk<'a>(nodes: &'a [Node], out: &mut Vec<&'a Node>) {
        for n in nodes {
            out.push(n);
            match n {
                Node::Box { children, .. } | Node::Quote(children) => walk(children, out),
                Node::Row { cells, .. } | Node::Columns { cells, .. } => cells.iter().for_each(|(_, k)| walk(k, out)),
                Node::List { items, .. } => items.iter().for_each(|k| walk(k, out)),
                _ => {}
            }
        }
    }
    let mut out = Vec::new();
    walk(nodes, &mut out);
    out
}

fn paras(doc: &Doc) -> Vec<&Para> {
    all(&doc.nodes).into_iter().filter_map(|n| if let Node::Para(p) = n { Some(p) } else { None }).collect()
}

/// The paragraph whose text starts with `start`.
fn para<'a>(doc: &'a Doc, start: &str) -> &'a Para {
    paras(doc).into_iter().find(|p| p.text().trim_start().starts_with(start)).unwrap_or_else(|| panic!("no paragraph starting {start:?} in\n{}", html::plain(doc)))
}

fn boxes(doc: &Doc) -> Vec<(&BoxStyle, &Vec<Node>)> {
    all(&doc.nodes).into_iter().filter_map(|n| if let Node::Box { style, children } = n { Some((style, children)) } else { None }).collect()
}

/// The innermost box holding a paragraph that starts with `start`.
fn box_of<'a>(doc: &'a Doc, start: &str) -> &'a BoxStyle {
    boxes(doc)
        .into_iter()
        .rfind(|(_, kids)| all(kids).iter().any(|n| matches!(n, Node::Para(p) if p.text().trim_start().starts_with(start))))
        .map(|(s, _)| s)
        .unwrap_or_else(|| panic!("no box around {start:?}"))
}

/// Every box around a paragraph that starts with `start`, outermost first.
fn boxes_around<'a>(doc: &'a Doc, start: &str) -> Vec<&'a BoxStyle> {
    boxes(doc).into_iter().filter(|(_, kids)| all(kids).iter().any(|n| matches!(n, Node::Para(p) if p.text().trim_start().starts_with(start)))).map(|(s, _)| s).collect()
}

/// Buttons: linked boxes, with their label.
fn buttons(doc: &Doc) -> Vec<(&BoxStyle, String)> {
    boxes(doc).into_iter().filter(|(s, _)| s.href.is_some()).map(|(s, kids)| (s, html::plain(&Doc { nodes: kids.clone(), ..Doc::default() }))).collect()
}

fn rows(doc: &Doc) -> Vec<&Vec<(BoxStyle, Vec<Node>)>> {
    all(&doc.nodes).into_iter().filter_map(|n| if let Node::Row { cells, .. } = n { Some(cells) } else { None }).collect()
}

fn columns(doc: &Doc) -> Vec<&Vec<(BoxStyle, Vec<Node>)>> {
    all(&doc.nodes).into_iter().filter_map(|n| if let Node::Columns { cells, .. } = n { Some(cells) } else { None }).collect()
}

fn pct(s: &BoxStyle) -> f32 {
    match s.width {
        Some(Len::Pct(p)) => p,
        other => panic!("not a share: {other:?}"),
    }
}

fn cell_text(cell: &(BoxStyle, Vec<Node>)) -> String {
    html::plain(&Doc { nodes: cell.1.clone(), ..Doc::default() })
}

const fn rgb(r: u8, g: u8, b: u8) -> Option<[u8; 4]> {
    Some([r, g, b, 255])
}

#[test]
fn every_fixture_parses_and_keeps_its_text() {
    let dir = format!("{}/tests/fixtures/html", env!("CARGO_MANIFEST_DIR"));
    let mut n = 0;
    for e in std::fs::read_dir(&dir).unwrap() {
        let p = e.unwrap().path();
        if p.extension().is_some_and(|x| x == "html") {
            let doc = html::parse(&std::fs::read_to_string(&p).unwrap());
            let text = html::plain(&doc);
            assert!(text.len() > 100, "{}: {text}", p.display());
            n += 1;
        }
    }
    assert!(n >= 20, "{n} fixtures");
}

#[test]
fn plain_personal() {
    let doc = load("plain-personal");
    assert_eq!(doc.fidelity.verdict(), Verdict::Fine);
    // Gmail's blank lines (`<div><br></div>`) stay blank lines.
    assert!(matches!(doc.nodes[1], Node::Blank(_)), "{:#?}", &doc.nodes[..3]);
    let list = all(&doc.nodes).into_iter().find_map(|n| if let Node::List { items, marker, .. } = n { Some((items.len(), *marker)) } else { None });
    assert_eq!(list, Some((3, true)));
    assert!(paras(&doc).iter().flat_map(|p| &p.runs).any(|r| r.href.as_deref() == Some("https://maps.example.org/route/coast-path") && r.underline));
    assert_eq!(para(&doc, "Jo Marsh").runs[0].color, rgb(102, 102, 102));
}

#[test]
fn gmail_reply_quotes_nest() {
    let doc = load("gmail-reply");
    let quotes = all(&doc.nodes).into_iter().filter(|n| matches!(n, Node::Quote(_))).count();
    assert_eq!(quotes, 2, "a reply quoting a reply");
    assert!(para(&doc, "Thanks Priya").runs.iter().any(|r| r.text == "Thursday 2pm" && r.bold()));
    assert!(all(&doc.nodes).iter().filter(|n| matches!(n, Node::Blank(_))).count() >= 3);
}

#[test]
fn outlook_word_html() {
    let doc = load("outlook-reply");
    // `p.MsoNormal { font-size: 11.0pt }` from the sheet; empty `<o:p>&nbsp;</o:p>` paragraphs
    // are blank lines.
    let hi = para(&doc, "Hi Lena");
    assert!((hi.runs[0].size.unwrap() - 14.67).abs() < 0.1, "{:?}", hi.runs[0].size);
    assert!(all(&doc.nodes).iter().filter(|n| matches!(n, Node::Blank(_))).count() >= 3);
    // The "From:" block's rule above, and the table's per-side borders.
    let from = box_of(&doc, "From:");
    assert_eq!(from.border.as_ref().map(|b| b.widths), Some([4.0 / 3.0, 0.0, 0.0, 0.0]));
    let r = rows(&doc);
    assert_eq!(r.len(), 2);
    assert!((pct(&r[0][0].0) - 28.6).abs() < 1.0, "90pt and 225pt columns: {}", pct(&r[0][0].0));
    let last = r[1][1].0.border.as_ref().unwrap();
    assert_eq!(last.widths[0], 0.0, "border-top: none");
    assert_eq!(last.widths[3], 0.0, "border-left: none");
    assert!(last.widths[1] > 0.0 && last.widths[2] > 0.0);
    assert_eq!(doc.fidelity.verdict(), Verdict::Fine);
}

#[test]
fn inline_receipt() {
    let doc = load("receipt-inline");
    let text = html::plain(&doc);
    assert!(!text.contains("Receipt #2231-9904 · $48.20 paid"), "preheader hidden");
    assert_eq!(doc.images, vec!["https://img.example.com/lumen/logo@2x.png"], "the tracking pixel isn't an image");
    let label = para(&doc, "RECEIPT FROM LUMEN CLOUD");
    assert!((label.tracking - 0.5).abs() < 0.01, "text-transform and letter-spacing");
    let amount = para(&doc, "$48.20");
    assert_eq!((amount.runs[0].size, amount.runs[0].weight), (Some(32.0), 600));
    assert_eq!(amount.line_height, Some(LineHeight::Px(40.0)));
    // Three summary columns from width="33%" / "33%" / "34%".
    let summary = rows(&doc).into_iter().find(|r| r.len() == 3).expect("summary row");
    let shares: Vec<f32> = summary.iter().map(|c| pct(&c.0)).collect();
    assert!((shares[0] - 33.0).abs() < 0.5 && (shares[2] - 34.0).abs() < 0.5, "{shares:?}");
    // Line items: amounts right-aligned in their own column, the same share in every row.
    let items: Vec<_> = rows(&doc).into_iter().filter(|r| r.len() == 2).collect();
    assert!(items.len() >= 5);
    let right = pct(&items[0][1].0);
    assert!(items.iter().all(|r| (pct(&r[1].0) - right).abs() < 0.01 && r[1].0.align == Align::Right));
    // The button: a padded link in a coloured cell.
    let (_, label) = buttons(&doc).into_iter().find(|(_, t)| t == "Download receipt").expect("button");
    assert_eq!(label, "Download receipt");
    let cell = boxes(&doc).into_iter().find(|(s, kids)| s.bg == rgb(0x63, 0x5b, 0xff) && kids.iter().any(|k| matches!(k, Node::Box { style, .. } if style.href.is_some()))).expect("button cell");
    assert!(cell.0.tight && cell.0.radius == 6.0);
}

#[test]
fn mjml_columns_hero_and_media_queries() {
    let doc = load("mjml-newsletter");
    assert_eq!(doc.fidelity.verdict(), Verdict::Fine);
    // The hero: a background picture over its fallback colour, drawn once.
    let heroes: Vec<_> = boxes(&doc).into_iter().filter(|(s, _)| s.bg_image.is_some()).collect();
    assert_eq!(heroes.len(), 1, "the Outlook copy of the picture is dropped");
    assert_eq!(heroes[0].0.bg, rgb(0x1b, 0x43, 0x32));
    assert!(doc.images.iter().any(|u| u == "https://img.example.com/fieldnotes/hero-seedlings.jpg"));
    assert!(!doc.images.iter().any(|u| u.contains("open.example.com")), "tracking pixel");
    let title = para(&doc, "Your spring planting guide");
    assert_eq!((title.family, title.runs[0].size, title.line_height), (Family::Serif, Some(34.0), Some(LineHeight::Px(40.0))));
    // `.mj-column-per-50 { width: 50% !important }` in `@media (min-width: 480px)` beats the
    // inline `width: 100%`: two columns, then three.
    let cols = columns(&doc);
    let shares: Vec<Vec<f32>> = cols.iter().map(|c| c.iter().map(|(s, _)| pct(s)).collect()).collect();
    assert!(shares.iter().any(|s| s.len() == 2 && s.iter().all(|p| (p - 50.0).abs() < 0.01)), "{shares:?}");
    assert!(shares.iter().any(|s| s.len() == 3 && s.iter().all(|p| (p - 33.33).abs() < 0.01)), "{shares:?}");
    // `.headline div { letter-spacing: 1px !important }` overrides the inline 3px.
    assert!((para(&doc, "FIELD NOTES").tracking - 1.0).abs() < 0.01);
    // `.story-title a { color: … !important }` beats the UA link colour; the button is a pill.
    assert_eq!(para(&doc, "A balcony").runs[0].color, rgb(0x1b, 0x43, 0x32));
    let (b, _) = buttons(&doc).into_iter().find(|(_, t)| t == "Read the guide").expect("button");
    assert_eq!((b.bg, b.radius, b.tight), (rgb(0xff, 0xd1, 0x66), 24.0, true));
}

#[test]
fn shipping_notice_sheet_colspan_rowspan() {
    let doc = load("shipping-colspan");
    // Class, id, type and descendant selectors.
    assert_eq!(box_of(&doc, "Harbor & Pine").bg, rgb(0x0b, 0x3d, 0x91));
    let title = para(&doc, "SHIPMENT DETAILS");
    assert!(title.runs[0].bold() && (title.tracking - 1.0).abs() < 0.01);
    assert_eq!(box_of(&doc, "SHIPMENT DETAILS").border.as_ref().map(|b| b.widths), Some([0.0, 0.0, 2.0, 0.0]));
    let (track, _) = buttons(&doc).into_iter().find(|(_, t)| t == "Track your package").expect("#track a");
    assert_eq!(track.bg, rgb(0xf5, 0xa6, 0x23));
    assert!(para(&doc, "Total").runs[0].bold(), ".totals .grand td");
    assert_eq!(para(&doc, "CX 4471").family, Family::Mono);
    // The address spans three rows down the last column: one row whose first cell holds the
    // three rows beside it.
    let band = rows(&doc).into_iter().find(|r| r.len() == 2 && cell_text(&r[1]).contains("Sam Rivera")).expect("rowspan band");
    assert!((pct(&band[1].0) - 25.0).abs() < 0.5, "{}", pct(&band[1].0));
    assert_eq!(all(&band[0].1).iter().filter(|n| matches!(n, Node::Row { .. })).count(), 3);
    // "Item" spans the image and description columns: its share is theirs together.
    let items = rows(&doc).into_iter().filter(|r| r.iter().any(|c| cell_text(c).contains("Tidewater") || cell_text(c) == "Item")).collect::<Vec<_>>();
    let head = items.iter().find(|r| cell_text(&r[0]) == "Item").expect("header");
    let item = items.iter().find(|r| r.len() == 4).expect("item row");
    assert!((pct(&head[0].0) - (pct(&item[0].0) + pct(&item[1].0))).abs() < 0.01);
    assert!(item[0].0.min_width >= 64.0, "the 64px image column keeps its width");
    assert_eq!(doc.fidelity.count(Issue::Rowspan), 0, "the band keeps its shape");
}

#[test]
fn dark_mode_rules_stay_light() {
    let doc = load("dark-mode-meta");
    assert_eq!(para(&doc, "Your week in review").runs[0].color, rgb(0x11, 0x18, 0x27), "the light rule, not the dark override");
    assert_eq!(doc.images, vec!["https://img.example.org/tally/logo-light.png"], "the dark logo stays hidden");
    assert_eq!(box_of(&doc, "Your week in review").bg, rgb(0xf4, 0xf6, 0xfb));
    let label = para(&doc, "TASKS CLOSED");
    assert!((label.tracking - 0.96).abs() < 0.01, "0.08em of 12px");
}

#[test]
fn unsupported_layout_is_flagged() {
    let doc = load("complex-unsupported");
    assert_eq!(doc.fidelity.verdict(), Verdict::Poor, "{:?}", doc.fidelity);
    for issue in [Issue::Positioned, Issue::FlexGrid, Issue::Svg, Issue::Embed, Issue::Form, Issue::Float, Issue::Effect] {
        assert!(doc.fidelity.count(issue) > 0, "{issue:?} in {:?}", doc.fidelity);
    }
    // Its text is all still there.
    let text = html::plain(&doc);
    assert!(text.contains("Ship faster with Relay") && text.contains("Usage alerts"));
}

#[test]
fn templates_score_fine() {
    for name in [
        "calendar-invite", "cerberus-fluid", "cerberus-hybrid", "cerberus-responsive", "dark-mode-meta", "foundation-hero", "gmail-reply", "mjml-newsletter", "munroe-responsive",
        "notification-code", "outlook-reply", "plain-personal", "postmark-comment", "postmark-invoice", "postmark-invoice-inlined", "postmark-password-reset", "postmark-receipt",
        "postmark-welcome", "receipt-inline", "shipping-colspan", "typography-newsletter",
    ] {
        let doc = load(name);
        assert_eq!(doc.fidelity.verdict(), Verdict::Fine, "{name}: {:?}", doc.fidelity);
    }
}

#[test]
fn code_notification() {
    let doc = load("notification-code");
    assert!(all(&doc.nodes).iter().any(|n| matches!(n, Node::Pre(t) if t.contains("Performance has improved."))));
    let p = para(&doc, "This allocates");
    let code = p.runs.iter().find(|r| r.text == "HashMap").expect("code run");
    assert!(code.mono && code.bg.is_some(), "{code:?}");
    assert!((para(&doc, "NEW COMMENT").tracking - 0.66).abs() < 0.01, ".06em of 11px");
    let card = box_of(&doc, "okafor-sam");
    assert!(card.border.is_some());
    assert_eq!(card.radius, 6.0);
}

#[test]
fn calendar_rsvp_pills() {
    let doc = load("calendar-invite");
    let pills: Vec<_> = buttons(&doc).into_iter().filter(|(_, t)| ["Yes", "No", "Maybe"].contains(&t.as_str())).collect();
    assert_eq!(pills.len(), 3);
    assert!(pills.iter().all(|(s, _)| s.radius == 16.0 && s.border.is_some() && s.bg.is_none()));
    let rsvp = rows(&doc).into_iter().find(|r| r.len() == 4).expect("RSVP row");
    assert_eq!(cell_text(&rsvp[0]), "Going?");
    let when = rows(&doc).into_iter().find(|r| cell_text(&r[0]) == "When").expect("When row");
    assert!(pct(&when[0].0) < 25.0, "the 88px label column");
}

#[test]
fn typography() {
    let doc = load("typography-newsletter");
    let name = para(&doc, "The Long Read");
    assert_eq!((name.family, name.runs[0].size, name.runs[0].weight), (Family::Serif, Some(40.0), 700));
    assert!((name.tracking + 0.5).abs() < 0.01);
    let dek = para(&doc, "When the motorway");
    assert!(dek.runs[0].italic && dek.family == Family::Serif);
    let byline = para(&doc, "BY HANNAH OKORO");
    assert_eq!(byline.family, Family::Sans);
    assert!(byline.runs.iter().any(|r| r.weight == 300 && r.text.contains("14 MIN READ")), "{:?}", byline.runs);
    let body = para(&doc, "In 1984");
    assert_eq!(body.line_height, Some(LineHeight::Px(30.0)));
    assert!(para(&doc, "Today, footfall").runs.iter().any(|r| r.text.contains("RENTS")), "text-transform: uppercase");
    let pull = box_of(&doc, "“We didn't");
    assert_eq!(pull.border.as_ref().map(|b| (b.widths, b.color)), Some(([0.0, 0.0, 0.0, 4.0], [0x9a, 0x3b, 0x26, 255])));
    assert!(boxes(&doc).iter().any(|(s, _)| s.max_width == Some(640.0)), "#wrapper");
}

#[test]
fn cerberus_fluid_columns_and_background() {
    let doc = load("cerberus-hybrid");
    let cols = columns(&doc);
    assert!(cols.iter().any(|c| c.len() == 2 && c.iter().all(|(s, _)| s.max_width == Some(330.0) && s.min_width == 200.0)));
    assert!(cols.iter().any(|c| c.len() == 3 && c.iter().all(|(s, _)| s.max_width == Some(220.0) && s.min_width == 160.0)));
    let bg = boxes(&doc).into_iter().find(|(s, _)| s.bg_image.is_some()).expect("background picture");
    assert_eq!(bg.0.bg_image.as_deref(), Some("https://via.placeholder.com/680x180/222222/666666"));
    assert_eq!(bg.0.bg, rgb(0x22, 0x22, 0x22));
    // `<center>` centres; the tables inside start over on the left, as browsers do.
    assert_eq!(para(&doc, "Praesent laoreet").align, Align::Left);
    assert_eq!(para(&doc, "View as a Web Page").align, Align::Center);
    // `dir="rtl"`: the second thumbnail row puts its picture last.
    let thumbs: Vec<_> = cols.iter().filter(|c| c.len() == 2 && c.iter().any(|(s, _)| s.max_width == Some(440.0))).collect();
    assert_eq!(thumbs.len(), 2);
    assert!(matches!(all(&thumbs[0][0].1).first(), Some(Node::Box { .. })) && cell_text(&thumbs[1][0]).starts_with("Class aptent"));
    // The floated button stays left of its column; the centred one is centred.
    let (_, b) = buttons(&doc).into_iter().find(|(_, t)| t == "Centered Primary Button").expect("button");
    assert_eq!(b, "Centered Primary Button");
}

#[test]
fn cerberus_responsive_desktop_rules() {
    let doc = load("cerberus-responsive");
    // `.stack-column { display: block; width: 100% }` is a mobile rule: on a desktop pane the
    // columns stay side by side.
    let r = rows(&doc);
    assert!(r.iter().any(|r| r.len() == 2 && r.iter().all(|c| (pct(&c.0) - 50.0).abs() < 1.0)), "{:?}", r.iter().map(|r| r.len()).collect::<Vec<_>>());
    assert!(r.iter().any(|r| r.len() == 3));
}

#[test]
fn postmark_receipt_classes() {
    let doc = load("postmark-receipt");
    let text = html::plain(&doc);
    assert!(!text.contains("This is a receipt for your recent purchase"), ".preheader {{ display: none }}");
    // Body text starts left: the wrapper's `align="center"` stops at the nested table.
    assert_eq!(para(&doc, "Thanks for using").align, Align::Left);
    assert_eq!(para(&doc, "10% off").align, Align::Center);
    assert_eq!(para(&doc, "Description").align, Align::Left, "<th align=left> beats th's centring");
    assert_eq!(para(&doc, "Amount").align, Align::Right);
    let thanks = para(&doc, "Thanks for using");
    assert_eq!((thanks.runs[0].size, thanks.line_height, thanks.runs[0].color), (Some(16.0), Some(LineHeight::Mult(1.625)), rgb(0x51, 0x54, 0x5e)));
    // The 570px card, the dashed coupon, the bulletproof button (borders become padding).
    assert!(boxes(&doc).iter().any(|(s, _)| s.width == Some(Len::Px(570.0)) && s.bg == rgb(255, 255, 255)));
    assert!(boxes_around(&doc, "10% off").iter().any(|s| s.border.as_ref().is_some_and(|b| b.dashed && b.widths == [2.0; 4]) && s.bg == rgb(0xf4, 0xf4, 0xf7)), "the dashed coupon");
    let (b, _) = buttons(&doc).into_iter().find(|(_, t)| t.starts_with("Use this discount")).expect("button");
    assert_eq!((b.bg, b.pad, b.radius), (rgb(0x22, 0xbc, 0x66), [10.0, 18.0, 10.0, 18.0], 3.0));
    // Items on an 80/20 grid; `colspan="2"` puts the item table across both header columns.
    let items: Vec<_> = rows(&doc).into_iter().filter(|r| r.len() == 2 && cell_text(&r[1]).starts_with('$')).collect();
    assert_eq!(items.len(), 4);
    assert!(items.iter().all(|r| (pct(&r[0].0) - 80.0).abs() < 0.01));
    assert_eq!(items[0][1].0.align, Align::Right);
}

#[test]
fn postmark_inlined_and_classes_agree() {
    // The same invoice, styled from a sheet and with its styles inlined: the same layout.
    let sheet = load("postmark-invoice");
    let inlined = load("postmark-invoice-inlined");
    assert_eq!(html::plain(&sheet), html::plain(&inlined));
    let shape = |d: &Doc| (rows(d).len(), buttons(d).len(), boxes(d).iter().filter(|(s, _)| s.bg.is_some()).count());
    assert_eq!(shape(&sheet), shape(&inlined));
    let style = |d: &Doc| {
        let p = para(d, "Thanks for using");
        (p.align, p.runs[0].size, p.runs[0].color, p.line_height)
    };
    assert_eq!(style(&sheet), style(&inlined));
}

#[test]
fn postmark_welcome_and_reset() {
    let doc = load("postmark-welcome");
    assert!(boxes_around(&doc, "Login Page:").iter().any(|s| s.bg == rgb(0xf4, 0xf4, 0xf7) && s.pad == [16.0; 4]), ".attributes_content");
    assert!(buttons(&doc).iter().any(|(s, t)| t == "Do this Next" && s.bg == rgb(0x38, 0x69, 0xd4)));
    let doc = load("postmark-password-reset");
    assert!(buttons(&doc).iter().any(|(s, t)| t == "Reset your password" && s.bg == rgb(0x22, 0xbc, 0x66)));
    let doc = load("postmark-comment");
    assert!(html::plain(&doc).contains("Sam Okafor"));
}

#[test]
fn foundation_grid() {
    let doc = load("foundation-hero");
    assert!(boxes_around(&doc, "HERO").iter().any(|s| s.bg == rgb(0x8a, 0x8a, 0x8a)), ".header {{ background }}");
    assert!(boxes(&doc).iter().any(|(s, _)| s.width == Some(Len::Px(580.0))), "table.container");
    // `th.large-6` columns: an even split.
    let r = rows(&doc).into_iter().find(|r| r.len() == 2 && cell_text(&r[0]).contains("Connect With Us")).expect("columns");
    assert!((pct(&r[0].0) - 50.0).abs() < 1.0, "{}", pct(&r[0].0));
    // `.button.large.secondary`: the padded link (its `href="#"` goes nowhere, so it's no
    // link here) in a bordered, coloured cell.
    let around = boxes_around(&doc, "Click Me!");
    assert!(around.iter().any(|s| s.pad == [10.0, 20.0, 10.0, 20.0] && s.tight), "{around:#?}");
    assert!(around.iter().any(|s| s.bg == rgb(0x77, 0x77, 0x77)));
    // `.menu.vertical .menu-item { display: block }` stacks the menu.
    let menu = ["Twitter", "Facebook", "Google +"];
    assert!(!rows(&doc).iter().any(|r| r.iter().filter(|c| menu.contains(&cell_text(c).as_str())).count() > 1), "stacked, not side by side");
    assert!(boxes_around(&doc, "Lorem ipsum dolor sit amet, consectetur adipisicing elit. Veniam").iter().any(|s| s.bg.is_some() && s.border.is_some()), "the callout");
}

#[test]
fn munroe_classes() {
    let doc = load("munroe-responsive");
    let (b, _) = buttons(&doc).into_iter().find(|(_, t)| t == "Call To Action").expect("button");
    assert_eq!(b.bg, rgb(0x08, 0x67, 0xec));
    assert!(b.tight, "the button keeps its own size inside the full-width .btn table");
    let card = boxes(&doc).into_iter().find(|(s, _)| s.radius == 16.0).expect("card");
    assert_eq!(card.0.bg, rgb(255, 255, 255));
    assert!(card.0.border.is_some());
}

/// A newsletter far bigger than real ones (a 300-rule sheet, 600 blocks of nested tables):
/// parsing stays bounded.
#[test]
fn huge_newsletter_is_bounded() {
    let mut css = String::new();
    for i in 0..300 {
        css.push_str(&format!(".c{i} td p span, .k{i} > table td.x{i} a:not(.y) {{ color: #{:06x}; padding: {}px }}\n", i * 997, i % 20));
    }
    let block = r#"<table width="100%" class="c1 k2"><tr><td class="x2"><table><tr><td><p class="c3"><span>Story headline</span> and a <a href="https://example.com/s">link</a></p></td><td width="120"><img src="https://img.example.com/s.jpg" width="120" height="80"></td></tr></table></td></tr></table>"#;
    let html = format!("<html><head><style>{css}</style></head><body>{}</body></html>", block.repeat(600));
    let t = std::time::Instant::now();
    let doc = html::parse(&html);
    let took = t.elapsed();
    assert_eq!(rows(&doc).len(), 600);
    assert!(took.as_secs_f64() < 10.0, "took {took:?}");
}
