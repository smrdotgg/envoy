//! Prints the layout tree and plain text of an HTML file:
//! `cargo run -p envoy-core --example html_dump -- mail.html` (`TREE=1` for the tree as an
//! outline, `TREE=debug` for every field).
use envoy_core::html::{BoxStyle, Len, Node};

fn main() {
    let path = std::env::args().nth(1).expect("path");
    let html = std::fs::read_to_string(path).unwrap();
    let t = std::time::Instant::now();
    let doc = envoy_core::html::parse(&html);
    eprintln!("parsed in {:?}, {} top nodes, images {:?}", t.elapsed(), doc.nodes.len(), doc.images);
    eprintln!("fidelity: score {} ({:?}) {:?}", doc.fidelity.score(), doc.fidelity.verdict(), doc.fidelity.issues);
    match std::env::var("TREE").as_deref() {
        Ok("debug") => println!("{:#?}", doc.nodes),
        Ok(_) => outline(&doc.nodes, 0),
        Err(_) => println!("{}", envoy_core::html::plain(&doc)),
    }
}

fn boxed(s: &BoxStyle) -> String {
    let mut parts = Vec::new();
    if s.align != Default::default() {
        parts.push(format!("{:?}", s.align));
    }
    if let Some(bg) = s.bg {
        parts.push(format!("bg #{:02x}{:02x}{:02x}", bg[0], bg[1], bg[2]));
    }
    if let Some(u) = &s.bg_image {
        parts.push(format!("bg-image {u}"));
    }
    if s.pad != [0.0; 4] {
        parts.push(format!("pad {:?}", s.pad));
    }
    if s.margin != [0.0; 4] {
        parts.push(format!("margin {:?}", s.margin));
    }
    match s.width {
        Some(Len::Px(w)) => parts.push(format!("w {w}px")),
        Some(Len::Pct(p)) => parts.push(format!("w {p:.1}%")),
        None => {}
    }
    if let Some(m) = s.max_width {
        parts.push(format!("max {m}"));
    }
    if s.min_width > 0.0 {
        parts.push(format!("min {}", s.min_width));
    }
    if let Some(b) = &s.border {
        parts.push(format!("border {:?}", b.widths));
    }
    if s.radius > 0.0 {
        parts.push(format!("r {}", s.radius));
    }
    if s.href.is_some() {
        parts.push("link".into());
    }
    if s.tight {
        parts.push("tight".into());
    }
    if s.stack {
        parts.push("stacks on phones".into());
    }
    parts.join(", ")
}

fn outline(nodes: &[Node], depth: usize) {
    let pad = "  ".repeat(depth);
    for n in nodes {
        match n {
            Node::Para(p) => {
                let t = p.text();
                let short: String = t.chars().take(70).collect();
                let r = &p.runs[0];
                println!("{pad}P {:?} {:?} size {:?} w{} lh {:?} \"{}\"", p.align, p.family, r.size, r.weight, p.line_height, short.replace('\n', "⏎"));
            }
            Node::Box { style, children } => {
                println!("{pad}Box [{}]", boxed(style));
                outline(children, depth + 1);
            }
            Node::Row { cells, gap, .. } => {
                println!("{pad}Row gap {gap}");
                for (s, kids) in cells {
                    println!("{pad}  Cell [{}]", boxed(s));
                    outline(kids, depth + 2);
                }
            }
            Node::Columns { cells, align, rtl } => {
                println!("{pad}Columns {align:?}{}", if *rtl { " rtl" } else { "" });
                for (s, kids) in cells {
                    println!("{pad}  Col [{}]", boxed(s));
                    outline(kids, depth + 2);
                }
            }
            Node::Image(i) => println!("{pad}Img {:?}x{:?} {}", i.width, i.height, i.src),
            Node::Rule => println!("{pad}Rule"),
            Node::Space(h) => println!("{pad}Space {h}"),
            Node::Blank(h) => println!("{pad}Blank {h}"),
            Node::List { ordered, marker, items } => {
                println!("{pad}List ordered={ordered} marker={marker}");
                for it in items {
                    println!("{pad}  •");
                    outline(it, depth + 2);
                }
            }
            Node::Quote(kids) => {
                println!("{pad}Quote");
                outline(kids, depth + 1);
            }
            Node::Pre(t) => println!("{pad}Pre {} lines", t.lines().count()),
        }
    }
}
