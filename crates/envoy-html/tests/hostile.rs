//! Mail is attacker-controlled. The parser runs on the engine's threads (2 MiB stacks) while
//! the store is locked, and the sanitizer behind "Open in browser" on the engine's thread, so
//! no input may panic either, overflow its stack or make it take time out of proportion to
//! its size; and nothing active may survive sanitizing.

use std::time::{Duration, Instant};

/// The engine's worker threads have 2 MiB stacks (tokio's default); so do these.
const STACK: usize = 2 << 20;

/// Generous for unoptimized test builds on slow CI runners; the bugs this catches took
/// minutes, overflowed the stack or panicked.
const BOUND: Duration = Duration::from_secs(8);

fn parse_bounded(name: &str, html: String) -> Duration {
    let len = html.len();
    if std::env::var_os("HOSTILE_VERBOSE").is_some() {
        eprintln!("{name} ({len} bytes)");
    }
    let t = std::thread::Builder::new()
        .stack_size(STACK)
        .spawn(move || {
            let t = Instant::now();
            let doc = envoy_html::parse(&html);
            // The tree is usable afterwards too.
            let _ = envoy_html::plain(&doc);
            let safe = envoy_html::export(&html, false, &mut |_| None);
            if let Some(active) = active_content(&safe) {
                panic!("{active} survived sanitizing:\n{safe}");
            }
            t.elapsed()
        })
        .unwrap()
        .join()
        .unwrap_or_else(|_| panic!("{name}: the parser panicked"));
    if std::env::var_os("HOSTILE_VERBOSE").is_some() {
        eprintln!("    {t:?}");
    }
    assert!(t < BOUND, "{name} ({len} bytes) took {t:?}");
    t
}

/// Anything a browser would run or load in sanitized output, read back as a browser would.
fn active_content(html: &str) -> Option<String> {
    use html5ever::tendril::TendrilSink;
    use markup5ever_rcdom::{NodeData, RcDom};
    let dom = html5ever::parse_document(RcDom::default(), Default::default()).one(html);
    let mut stack = vec![dom.document.clone()];
    let mut policy = false;
    while let Some(n) = stack.pop() {
        if let NodeData::Element { name, attrs, .. } = &n.data {
            let tag = name.local.as_ref();
            if ["script", "iframe", "frame", "object", "embed", "applet", "base", "link", "form", "input", "noscript"].contains(&tag) {
                return Some(format!("<{tag}>"));
            }
            for a in attrs.borrow().iter() {
                let k = a.name.local.as_ref().to_ascii_lowercase();
                let v: String = a.value.chars().filter(|c| !c.is_ascii_whitespace() && !c.is_ascii_control()).collect::<String>().to_ascii_lowercase();
                if k.starts_with("on") || k == "srcset" || k == "action" || k == "ping" {
                    return Some(format!("{tag} {k}="));
                }
                if ["href", "src", "background"].contains(&k.as_str()) && (v.starts_with("javascript:") || v.starts_with("vbscript:") || v.starts_with("http") && k != "href") {
                    return Some(format!("{tag} {k}={v}"));
                }
                if tag == "meta" && k == "http-equiv" {
                    if v != "content-security-policy" {
                        return Some(format!("meta http-equiv={v}"));
                    }
                    policy = true;
                }
            }
        }
        stack.extend(n.children.borrow().iter().cloned());
    }
    (!policy).then(|| "no Content-Security-Policy".to_owned())
}

fn page(css: &str, body: &str) -> String {
    format!("<html><head><style>{css}</style></head><body>{body}</body></html>")
}

#[test]
fn adversarial_inputs() {
    let numbers = ["0", "-1", "-0", "2147483647", "-2147483648", "2147483648", "4294967296", "18446744073709551616", "99999999999999999999999999", "1e38", "1e40", "-1e40", "NaN", "inf", "-inf", "1.5", "0.0001", ""];
    let mut cases: Vec<(String, String)> = vec![
        ("nth-child at i32's ends".into(), page("p:nth-child(-n-2147483647){color:red} p:nth-child(-2147483648n-2147483648){color:red} p:nth-child(2147483647n+2147483647){color:red}", "<p>a</p><p>b</p>")),
        ("nested :not ×4000".into(), page(&format!("p{}a{} {{color:red}}", ":not(".repeat(4000), ")".repeat(4000)), "<p>x</p>")),
        ("nested :not ×40,000, unclosed".into(), page(&":not(".repeat(40_000), "<p>x</p>")),
        ("20k url() in a sheet".into(), page(&format!("p {{ background: {} }}", "URL(https://A.example/X.png) ".repeat(20_000)), "<p>x</p>")),
        ("20k url() inline".into(), format!("<p style=\"background:{}\">x</p>", "url(https://A.example/X.png) ".repeat(20_000))),
        ("a 1 MB selector".into(), page(&format!("{} {{color:red}}", "div ".repeat(250_000)), "<div><p>x</p></div>")),
        ("100k compound selector list".into(), page(&format!("{} {{color:red}}", "p.a,".repeat(100_000)), "<p class=a>x</p>")),
        ("100k declarations on *".into(), page(&format!("* {{ {} }}", "color: red; padding: 1px; ".repeat(50_000)), &"<div><p>x</p></div>".repeat(2_000))),
        ("8,000 rules the ancestor filter rejects".into(), page(&(0..8_000).map(|i| format!(".x{i} * {{color:red}}")).collect::<String>(), &"<div><span>x</span></div>".repeat(10_000))),
        ("8,000 universal rules".into(), page(&(0..8_000).map(|i| format!("* {{padding:{}px}}", i % 20)).collect::<String>(), &"<p>x</p>".repeat(5_000))),
        ("10,000 nested divs".into(), format!("{}x{}", "<div style='font-size:0;line-height:0'>".repeat(10_000), "</div>".repeat(10_000))),
        ("500 nested tables".into(), format!("{}x{}", "<table><tr><td>".repeat(500), "</td></tr></table>".repeat(500))),
        ("500 nested tables with columns".into(), format!("{}x{}", "<table width=100%><tr><td width=50%>a</td><td colspan=3 rowspan=7>".repeat(500), "</td></tr></table>".repeat(500))),
        ("2,000 nested blockquotes, lists and links".into(), format!("{}x{}", "<blockquote><ul><li><a href='https://e.x'><b>".repeat(2_000), "</b></a></li></ul></blockquote>".repeat(2_000))),
        ("10,000 cells × colspan 50 in one row".into(), format!("<table><tr>{}</tr><tr><td rowspan=50>x</td></tr></table>", "<td colspan=50 width=1e40>c</td>".repeat(10_000))),
        ("100,000 whitespace runs".into(), format!("<p>{}end</p>", "<b> </b><i> </i>".repeat(50_000))),
        ("@media ×1,000".into(), page(&format!("{}p{{color:red}}{}", "@media screen{".repeat(1_000), "}".repeat(1_000)), "<p>x</p>")),
        ("unclosed everything".into(), page("p { color: red; background: url(  ;; [[[ (((( {{{{ \"unterminated", &"<table><tr><td colspan='".repeat(1_000))),
        ("a 100,000-class element".into(), format!("<p class=\"{}\">x</p>", (0..100_000).map(|i| format!("c{i} ")).collect::<String>())),
        ("100,000 attributes".into(), format!("<p {}>x</p>", (0..100_000).map(|i| format!("a{i}=1 ")).collect::<String>())),
        ("10,000 unclosed formatting elements".into(), format!("{}{}", (0..10_000).map(|i| format!("<b class=x{i}>")).collect::<String>(), "text<p>".repeat(2_000))),
        ("misnested formatting ×20,000".into(), "<b><i><u><a href='https://e.x'>x</b></i></u></a>".repeat(20_000)),
    ];
    for n in numbers {
        cases.push((
            format!("numbers: {n:?}"),
            format!(
                "<style>p {{ width: {n}px; padding: {n}em; font-size: {n}; line-height: {n}; letter-spacing: {n}px; margin: {n}% auto; border: {n}px solid red; border-radius: {n}% }} p:nth-child({n}n+{n}) {{ color: red }}</style>\
                 <table width='{n}' cellpadding='{n}' cellspacing='{n}' border='{n}'><tr><td colspan='{n}' rowspan='{n}' width='{n}%' height='{n}'><p>x</p></td><td width='{n}'>y</td></tr>\
                 <tr><td colspan='{n}'>z</td></tr></table><img src='https://i.x/a.png' width='{n}' height='{n}'><font size='{n}'>f</font><ol start='{n}'><li>l</li></ol>"
            ),
        ));
    }
    let mut slowest = (Duration::ZERO, String::new());
    for (name, html) in cases {
        let t = parse_bounded(&name, html);
        if t > slowest.0 {
            slowest = (t, name);
        }
    }
    eprintln!("slowest adversarial case: {} in {:?}", slowest.1, slowest.0);
}

/// xorshift64*: deterministic, so a failure reproduces.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
    fn pick<'a>(&mut self, xs: &[&'a str]) -> &'a str {
        xs[(self.next() % xs.len() as u64) as usize]
    }
}

/// Random documents from pieces of real and broken email HTML and CSS.
#[test]
fn random_documents_never_panic() {
    let tags = [
        "div", "p", "span", "table", "tr", "td", "th", "tbody", "a", "b", "img", "ul", "li", "blockquote", "center", "font", "pre", "br", "hr", "h1", "svg", "form", "input", "iframe", "style", "o:p", "v:rect",
        "script", "object", "embed", "meta http-equiv=refresh content=0", "base href=https://e.x/", "noscript", "template",
    ];
    let attrs = [
        "colspan", "rowspan", "width", "height", "cellpadding", "cellspacing", "border", "align", "valign", "bgcolor", "background", "class", "id", "style", "src", "href", "size", "dir", "hidden",
        "onclick", "onload", "srcset", "action",
    ];
    let values = [
        "0", "-1", "2147483647", "-2147483648", "4294967296", "1e40", "NaN", "50%", "100%", "-5%", "center", "left", "rtl", "#fff", "red", "x", "a b c", "", "https://e.x/a.png", "cid:a@b", "javascript:alert(1)",
        "javascript&colon;", "display:none", "font-size:0;line-height:0", "width:100%;max-width:330px;display:inline-block", "float:left;width:50%", "position:absolute", "background:url(https://e.x/b.png) #123",
    ];
    let css = [
        "p", "*", ".a", "#b", "div > p", "a + b", "p ~ span", "td.x .y", "[class*=a]", ":not(.a)", ":not(:not(.a))", ":nth-child(-n-2147483647)", ":nth-child(2n+1)", "::before", ":hover", "@media (max-width: 600px) {",
        "@media screen {", "}", "{", ";", "color: red", "width: 1e40px", "padding: -5px", "font: 700 99999px/0 x", "background: url(", "url(https://E.x/Y.png)", "!important", "\"", "'", "(", ")", "[", "]", "/*", "*/",
        "<!--", "-->", "@supports (x) {", "@font-face {", "margin: 0 auto", "border: 1px dashed", "line-height: -1", "letter-spacing: 1e40em",
    ];
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let started = Instant::now();
    for round in 0..250 {
        let mut html = String::from("<style>");
        for _ in 0..(rng.next() % 60) {
            html.push_str(rng.pick(&css));
            html.push(' ');
        }
        html.push_str("</style>");
        let mut open: Vec<&str> = Vec::new();
        for _ in 0..(rng.next() % 400) {
            match rng.next() % 6 {
                0 if !open.is_empty() => {
                    let t = open.pop().unwrap();
                    html.push_str(&format!("</{t}>"));
                }
                1 => html.push_str(rng.pick(&["text ", "&nbsp;", "\u{200b}", " ", "\n", "x<y", "&amp;", "\u{a0}\u{a0}"])),
                _ => {
                    let t = rng.pick(&tags);
                    html.push('<');
                    html.push_str(t);
                    for _ in 0..(rng.next() % 4) {
                        html.push_str(&format!(" {}=\"{}\"", rng.pick(&attrs), rng.pick(&values)));
                    }
                    html.push('>');
                    if t == "style" {
                        html.push_str(rng.pick(&css));
                    }
                    open.push(t);
                }
            }
        }
        parse_bounded(&format!("random document {round}"), html);
    }
    assert!(started.elapsed() < Duration::from_secs(60), "250 random documents took {:?}", started.elapsed());
}
