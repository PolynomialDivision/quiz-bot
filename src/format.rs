use std::collections::HashMap;

use matrix_sdk::ruma::events::room::message::RoomMessageEventContent;

/// Scan `text` for Matrix user IDs and return a `RoomMessageEventContent`
/// with HTML mention pills showing the localpart as the pill label.
pub fn mentionify(text: &str) -> RoomMessageEventContent {
    build(text, |token| default_label(token))
}

/// Like `mentionify`, but looks up display names from `names`
/// (key = full MXID, value = display name) so the pill shows the
/// friendly name instead of the localpart.
/// The plain-text body is also updated: `@user:server` → `Name`.
pub fn mentionify_with_names(
    text: &str,
    names: &HashMap<String, String>,
) -> RoomMessageEventContent {
    build(text, |token| {
        names
            .get(token)
            .map(|s| s.as_str())
            .unwrap_or_else(|| default_label(token))
    })
}

// ── Internals ─────────────────────────────────────────────────────────────────

fn default_label(token: &str) -> &str {
    token
        .split(':')
        .next()
        .unwrap_or("")
        .trim_start_matches('@')
}

/// Build a `RoomMessageEventContent` by scanning `text` for MXIDs and
/// `**bold**` markers, replacing them for both the plain body and the HTML
/// body.  `label_for(mxid) -> &str` controls the pill label text.
fn build<'a>(text: &'a str, label_for: impl Fn(&'a str) -> &'a str) -> RoomMessageEventContent {
    let mut plain    = String::with_capacity(text.len());
    let mut html     = String::with_capacity(text.len() * 2);
    let mut pos      = 0;
    let mut found    = false;   // true when HTML output differs from plain
    let mut in_bold  = false;

    while pos < text.len() {
        // ── **bold** markers ──────────────────────────────────────────────────
        if text.as_bytes().get(pos) == Some(&b'*')
            && text.as_bytes().get(pos + 1) == Some(&b'*')
        {
            if in_bold {
                html.push_str("</strong>");
            } else {
                html.push_str("<strong>");
            }
            in_bold = !in_bold;
            found   = true;
            pos    += 2;
            continue;
        }

        // ── @user:server MXID pills ───────────────────────────────────────────
        if text.as_bytes()[pos] == b'@' {
            let token_len = text[pos..]
                .find(|c: char| {
                    c.is_whitespace()
                        || matches!(c, ',' | '!' | '?' | '*' | ')' | ']' | '"' | '\'')
                })
                .unwrap_or(text.len() - pos);

            let token = &text[pos..pos + token_len];

            if token.len() > 4 && token.contains(':') {
                let label = label_for(token);
                plain.push_str(label);
                html.push_str(&format!(
                    r#"<a href="https://matrix.to/#/{token}">{label}</a>"#
                ));
                found = true;
                pos += token_len;
                continue;
            }
        }

        // ── Regular character ─────────────────────────────────────────────────
        let ch = text[pos..].chars().next().unwrap();
        plain.push(ch);
        match ch {
            '&'  => html.push_str("&amp;"),
            '<'  => html.push_str("&lt;"),
            '>'  => html.push_str("&gt;"),
            '"'  => html.push_str("&quot;"),
            '\n' => html.push_str("<br>"),
            _    => html.push(ch),
        }
        pos += ch.len_utf8();
    }

    // Close any unclosed bold tag (shouldn't happen with well-formed input).
    if in_bold {
        html.push_str("</strong>");
    }

    if found {
        RoomMessageEventContent::text_html(plain, html)
    } else {
        RoomMessageEventContent::text_plain(text)
    }
}

#[cfg(test)]
mod tests {
    use matrix_sdk::ruma::events::room::message::MessageType;
    use super::*;

    /// Extract (plain_body, Option<html_body>) from a RoomMessageEventContent.
    fn bodies(c: &RoomMessageEventContent) -> (String, Option<String>) {
        match &c.msgtype {
            MessageType::Text(t) => (
                t.body.clone(),
                t.formatted.as_ref().map(|f| f.body.clone()),
            ),
            _ => panic!("unexpected msgtype"),
        }
    }

    #[test]
    fn replaces_single_mxid() {
        let c = mentionify("Hello @alice:example.org!");
        let (_, html) = bodies(&c);
        let html = html.expect("should have HTML body");
        assert!(html.contains(r#"href="https://matrix.to/#/@alice:example.org""#));
        assert!(html.contains(">alice<"));
    }

    #[test]
    fn replaces_multiple_mxids() {
        let c = mentionify("@a:x.org and @b:y.org");
        let (_, html) = bodies(&c);
        let html = html.expect("should have HTML body");
        assert!(html.contains(">a<"));
        assert!(html.contains(">b<"));
    }

    #[test]
    fn no_mxid_returns_plain() {
        let c = mentionify("no mentions here");
        let (_, html) = bodies(&c);
        assert!(html.is_none());
    }

    #[test]
    fn escapes_html_outside_mxid() {
        let c = mentionify("x < y & @u:s.org");
        let (_, html) = bodies(&c);
        let html = html.expect("should have HTML body");
        assert!(html.contains("&lt;"));
        assert!(html.contains("&amp;"));
    }

    #[test]
    fn bold_markers_become_strong() {
        let c = mentionify("Answer: **Paris**");
        let (plain, html) = bodies(&c);
        let html = html.expect("should have HTML body");
        assert!(html.contains("<strong>Paris</strong>"), "html={html}");
        assert!(!plain.contains('*'), "plain body={plain}");
        assert!(plain.contains("Paris"));
    }

    #[test]
    fn bold_and_mxid_together() {
        let c = mentionify("**@alice:example.org** got it right");
        let (_, html) = bodies(&c);
        let html = html.expect("should have HTML body");
        assert!(html.contains("<strong>"), "html={html}");
        assert!(html.contains(r#"href="https://matrix.to/#/@alice:example.org""#));
    }

    #[test]
    fn with_names_uses_display_name() {
        let mut names = HashMap::new();
        names.insert("@alice:example.org".to_owned(), "Alice Smith".to_owned());
        let c = mentionify_with_names("Hello @alice:example.org!", &names);
        let (_, html) = bodies(&c);
        let html = html.expect("should have HTML body");
        assert!(html.contains(">Alice Smith<"));
        assert!(html.contains(r#"href="https://matrix.to/#/@alice:example.org""#));
    }
}
