//! Prints the layout tree and plain text of an HTML file: `cargo run --example html_dump -- mail.html`
fn main() {
    let path = std::env::args().nth(1).expect("path");
    let html = std::fs::read_to_string(path).unwrap();
    let t = std::time::Instant::now();
    let doc = envoy_core::html::parse(&html);
    eprintln!("parsed in {:?}, {} top nodes, images {:?}", t.elapsed(), doc.nodes.len(), doc.images);
    if std::env::var("TREE").is_ok() { println!("{:#?}", doc.nodes); }
    println!("{}", envoy_core::html::plain(&doc));
}
