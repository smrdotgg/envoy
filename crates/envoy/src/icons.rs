//! Line icons on a 24-point grid, drawn in the text colour (`currentColor`).

use rune::Icon;

macro_rules! icon {
    ($id:ident, $name:literal, $body:literal) => {
        pub static $id: Icon = Icon {
            name: concat!("envoy-", $name),
            view: 24.0,
            body: concat!(
                r#"<g fill="none" stroke="currentColor" stroke-width="1.75" stroke-linecap="round" stroke-linejoin="round">"#,
                $body,
                "</g>"
            ),
        };
    };
}

icon!(INBOX, "inbox", r#"<path d="M3 13h5l1.5 3h5L16 13h5"/><path d="M5.5 5h13L21 13v5a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-5z"/>"#);
icon!(STAR, "star", r#"<path d="M12 3.5l2.6 5.3 5.9.9-4.25 4.1 1 5.8L12 16.9l-5.25 2.7 1-5.8L3.5 9.7l5.9-.9z"/>"#);
pub static STAR_FILLED: Icon = Icon {
    name: "envoy-star-filled",
    view: 24.0,
    body: r#"<path d="M12 3.5l2.6 5.3 5.9.9-4.25 4.1 1 5.8L12 16.9l-5.25 2.7 1-5.8L3.5 9.7l5.9-.9z" fill="currentColor" stroke="currentColor" stroke-width="1.75" stroke-linejoin="round"/>"#,
};
icon!(SEND, "send", r#"<path d="M21 3L10 14"/><path d="M21 3l-7 18-4-7-7-4z"/>"#);
icon!(DRAFT, "draft", r#"<path d="M14 3H7a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h10a2 2 0 0 0 2-2V8z"/><path d="M14 3v5h5"/><path d="M9 13h6M9 17h4"/>"#);
icon!(ARCHIVE, "archive", r#"<rect x="3" y="4" width="18" height="4.5" rx="1"/><path d="M5 8.5V19a1 1 0 0 0 1 1h12a1 1 0 0 0 1-1V8.5"/><path d="M10 12.5h4"/>"#);
icon!(TRASH, "trash", r#"<path d="M4 7h16"/><path d="M10 11v6M14 11v6"/><path d="M6 7l1 12a2 2 0 0 0 2 2h6a2 2 0 0 0 2-2l1-12"/><path d="M9 7V4.5a1 1 0 0 1 1-1h4a1 1 0 0 1 1 1V7"/>"#);
icon!(SPAM, "spam", r#"<path d="M8.2 3h7.6L21 8.2v7.6L15.8 21H8.2L3 15.8V8.2z"/><path d="M12 8v4.5"/><path d="M12 16h.01"/>"#);
icon!(MAIL, "mail", r#"<rect x="3" y="5" width="18" height="14" rx="2"/><path d="M3.5 6.5l8.5 6 8.5-6"/>"#);
icon!(MAIL_OPEN, "mail-open", r#"<path d="M3 10l9-6 9 6v9a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2z"/><path d="M3.5 10.5l8.5 5.5 8.5-5.5"/>"#);
icon!(MAIL_DOT, "mail-dot", r#"<path d="M14 5H5a2 2 0 0 0-2 2v10a2 2 0 0 0 2 2h14a2 2 0 0 0 2-2v-6"/><path d="M3.5 7.5l8.5 6 4-2.8"/><circle cx="19" cy="5.5" r="2.5"/>"#);
icon!(LABEL, "label", r#"<path d="M3 5.5v5.3a1.5 1.5 0 0 0 .44 1.06l8.2 8.2a1.5 1.5 0 0 0 2.12 0l5.3-5.3a1.5 1.5 0 0 0 0-2.12l-8.2-8.2A1.5 1.5 0 0 0 9.8 4H4.5A1.5 1.5 0 0 0 3 5.5z"/><circle cx="7.5" cy="8.5" r="1"/>"#);
icon!(IMPORTANT, "important", r#"<path d="M4 6h11l5 6-5 6H4l4-6z"/>"#);
icon!(PENCIL, "pencil", r#"<path d="M16.5 3.5a2.1 2.1 0 0 1 3 3L8 18l-4 1 1-4z"/><path d="M14.5 5.5l3 3"/>"#);
icon!(REPLY, "reply", r#"<path d="M9 14L4 9l5-5"/><path d="M4 9h10.5a5.5 5.5 0 0 1 5.5 5.5V20"/>"#);
icon!(REPLY_ALL, "reply-all", r#"<path d="M7 14L2 9l5-5"/><path d="M12 14L7 9l5-5"/><path d="M7 9h8.5a5.5 5.5 0 0 1 5.5 5.5V20"/>"#);
icon!(FORWARD, "forward", r#"<path d="M15 14l5-5-5-5"/><path d="M20 9H9.5A5.5 5.5 0 0 0 4 14.5V20"/>"#);
icon!(SEARCH, "search", r#"<circle cx="11" cy="11" r="7"/><path d="M20 20l-4-4"/>"#);
icon!(SETTINGS, "settings", r#"<path d="M4 7h10M18 7h2M4 17h4M12 17h8"/><circle cx="16" cy="7" r="2.2"/><circle cx="10" cy="17" r="2.2"/>"#);
icon!(REFRESH, "refresh", r#"<path d="M20 11a8 8 0 0 0-14.6-4.5L4 8"/><path d="M4 3.5V8h4.5"/><path d="M4 13a8 8 0 0 0 14.6 4.5L20 16"/><path d="M20 20.5V16h-4.5"/>"#);
icon!(PAPERCLIP, "paperclip", r#"<path d="M20.5 11.5l-8.2 8.2a5 5 0 0 1-7.1-7.1l8.6-8.6a3.4 3.4 0 0 1 4.8 4.8l-8.6 8.6a1.7 1.7 0 0 1-2.4-2.4l7.9-7.9"/>"#);
icon!(BACK, "back", r#"<path d="M15 18l-6-6 6-6"/>"#);
icon!(PLUS, "plus", r#"<path d="M12 5v14M5 12h14"/>"#);
icon!(MINUS, "minus", r#"<path d="M5 12h14"/>"#);
icon!(CLOSE, "close", r#"<path d="M6 6l12 12M18 6L6 18"/>"#);
icon!(EXTERNAL, "external", r#"<path d="M14 4h6v6"/><path d="M20 4l-9 9"/><path d="M18 14v4a2 2 0 0 1-2 2H6a2 2 0 0 1-2-2V8a2 2 0 0 1 2-2h4"/>"#);
icon!(DOWNLOAD, "download", r#"<path d="M12 4v11"/><path d="M7 10.5l5 5 5-5"/><path d="M4 20h16"/>"#);
icon!(OFFLINE, "offline", r#"<path d="M3 3l18 18"/><path d="M8.5 5.6A8 8 0 0 1 19 13.5"/><path d="M5.6 8.5A8 8 0 0 0 5 18h12"/>"#);
icon!(CHECK, "check", r#"<path d="M5 12.5l4.5 4.5L19 7.5"/>"#);
icon!(IMAGE, "image", r#"<rect x="3" y="4.5" width="18" height="15" rx="2"/><circle cx="8.5" cy="9.5" r="1.5"/><path d="M21 15.5l-5-5L5 19.5"/>"#);
icon!(ALERT, "alert", r#"<path d="M12 4L2.5 20h19z"/><path d="M12 10v4"/><path d="M12 17h.01"/>"#);
icon!(USER, "user", r#"<circle cx="12" cy="8" r="4"/><path d="M4.5 20.5a7.5 7.5 0 0 1 15 0"/>"#);
icon!(LAYERS, "layers", r#"<path d="M12 3l9 5-9 5-9-5z"/><path d="M3 13l9 5 9-5"/>"#);
icon!(FILE_CODE, "file-code", r#"<path d="M14 3H7a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h10a2 2 0 0 0 2-2V8z"/><path d="M14 3v5h5"/><path d="M10 12l-2 2 2 2M14 12l2 2-2 2"/>"#);
icon!(KEYBOARD, "keyboard", r#"<rect x="2.5" y="6" width="19" height="12" rx="2"/><path d="M6 10h.01M10 10h.01M14 10h.01M18 10h.01M8 14h8"/>"#);
icon!(PALETTE, "palette", r#"<path d="M12 3a9 9 0 0 0 0 18c1.1 0 1.8-.8 1.8-1.8 0-.5-.2-.9-.5-1.2-.3-.3-.5-.7-.5-1.2 0-1 .8-1.8 1.8-1.8H17a4 4 0 0 0 4-4c0-4.4-4-8-9-8z"/><circle cx="7.5" cy="11" r="1"/><circle cx="10.5" cy="7" r="1"/><circle cx="15" cy="7.5" r="1"/>"#);
icon!(BOOK, "book", r#"<path d="M4 19.5V5a2 2 0 0 1 2-2h14v15H6a2 2 0 0 0-2 2z"/><path d="M4 19.5A1.5 1.5 0 0 0 5.5 21H20"/>"#);
icon!(BOLT, "bolt", r#"<path d="M13 2L4 14h7l-1 8 9-12h-7z"/>"#);
icon!(SLIDERS, "sliders", r#"<path d="M4 21v-7M4 10V3M12 21v-9M12 8V3M20 21v-5M20 12V3M1 14h6M9 8h6M17 16h6"/>"#);
