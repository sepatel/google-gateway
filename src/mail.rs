use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::{Value, json};

pub fn header(payload: &Value, name: &str) -> Option<String> {
    payload["headers"]
        .as_array()?
        .iter()
        .find(|h| {
            h["name"]
                .as_str()
                .is_some_and(|n| n.eq_ignore_ascii_case(name))
        })
        .and_then(|h| h["value"].as_str())
        .map(str::to_string)
}

pub fn render_message(message: &Value, body_limit: Option<usize>) -> Value {
    let payload = &message["payload"];
    let mut rendered = json!({
        "id": message["id"],
        "thread_id": message["threadId"],
        "date": header(payload, "Date"),
        "from": header(payload, "From"),
        "to": header(payload, "To"),
        "cc": header(payload, "Cc"),
        "subject": header(payload, "Subject"),
        "labels": message["labelIds"],
        "snippet": message["snippet"],
    });

    let Some(limit) = body_limit else {
        return rendered;
    };
    let body = extract_body(payload).unwrap_or_default();
    let (body, truncated) = truncate(&body, limit);
    rendered["body"] = json!(body);
    if truncated {
        rendered["body_truncated"] = json!(true);
    }
    let attachments = attachments(payload);
    if !attachments.is_empty() {
        rendered["attachments"] = json!(attachments);
    }
    rendered
}

/// The readable text of a message: the first plain-text part, else the first HTML part as text.
pub fn extract_body(payload: &Value) -> Option<String> {
    let mut parts = Vec::new();
    collect_parts(payload, &mut parts);
    let text_of = |mime: &str| -> Option<String> {
        parts
            .iter()
            .filter(|p| p["filename"].as_str().is_none_or(str::is_empty))
            .find(|p| p["mimeType"].as_str() == Some(mime))
            .and_then(|p| decode(p["body"]["data"].as_str()?))
    };
    text_of("text/plain")
        .filter(|t| !t.trim().is_empty())
        .or_else(|| text_of("text/html").map(|html| html_to_text(&html)))
}

fn collect_parts<'a>(part: &'a Value, out: &mut Vec<&'a Value>) {
    out.push(part);
    for child in part["parts"].as_array().into_iter().flatten() {
        collect_parts(child, out);
    }
}

fn attachments(payload: &Value) -> Vec<Value> {
    let mut parts = Vec::new();
    collect_parts(payload, &mut parts);
    parts
        .into_iter()
        .filter(|p| p["filename"].as_str().is_some_and(|f| !f.is_empty()))
        .map(|p| {
            json!({
                "filename": p["filename"],
                "mime_type": p["mimeType"],
                "size": p["body"]["size"],
            })
        })
        .collect()
}

fn decode(data: &str) -> Option<String> {
    // Gmail sends base64url, sometimes padded.
    let bytes = URL_SAFE_NO_PAD.decode(data.trim_end_matches('=')).ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

pub fn truncate(text: &str, max_chars: usize) -> (String, bool) {
    match text.char_indices().nth(max_chars) {
        Some((cut, _)) => (text[..cut].to_string(), true),
        None => (text.to_string(), false),
    }
}

pub fn html_to_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len() / 2);
    let mut rest = html;
    while let Some(open) = rest.find('<') {
        out.push_str(&decode_entities(&rest[..open]));
        let Some(close) = rest[open..].find('>') else {
            rest = "";
            break;
        };
        let tag = rest[open + 1..open + close].trim().to_ascii_lowercase();
        rest = &rest[open + close + 1..];
        let name = tag
            .trim_start_matches('/')
            .split(|c: char| c.is_whitespace() || c == '/')
            .next()
            .unwrap_or_default();
        if !tag.starts_with('/') && matches!(name, "script" | "style" | "head") {
            let end = format!("</{name}");
            rest = rest
                .to_ascii_lowercase()
                .find(&end)
                .and_then(|i| rest[i..].find('>').map(|j| &rest[i + j + 1..]))
                .unwrap_or("");
            continue;
        }
        if matches!(
            name,
            "br" | "p" | "div" | "tr" | "li" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "table"
        ) {
            out.push('\n');
        }
    }
    out.push_str(&decode_entities(rest));
    tidy_whitespace(&out)
}

fn decode_entities(text: &str) -> String {
    if !text.contains('&') {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        rest = &rest[amp..];
        let decoded = rest.find(';').filter(|&semi| semi <= 10).and_then(|semi| {
            let entity = &rest[1..semi];
            let ch = match entity {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" | "#39" => Some('\''),
                "nbsp" => Some(' '),
                _ => entity
                    .strip_prefix("#x")
                    .and_then(|hex| u32::from_str_radix(hex, 16).ok())
                    .or_else(|| entity.strip_prefix('#').and_then(|d| d.parse().ok()))
                    .and_then(char::from_u32),
            };
            ch.map(|c| (c, semi))
        });
        match decoded {
            Some((c, semi)) => {
                out.push(c);
                rest = &rest[semi + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

fn tidy_whitespace(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut blank_run = 0;
    for line in text.lines() {
        let line = line.split_whitespace().collect::<Vec<_>>().join(" ");
        if line.is_empty() {
            blank_run += 1;
            if blank_run > 1 {
                continue;
            }
        } else {
            blank_run = 0;
        }
        out.push_str(&line);
        out.push('\n');
    }
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::URL_SAFE;

    fn b64(s: &str) -> String {
        URL_SAFE.encode(s)
    }

    #[test]
    fn prefers_the_plain_text_part_and_lists_attachments() {
        let payload = json!({
            "mimeType": "multipart/mixed",
            "headers": [{"name": "subject", "value": "FedRAMP update"}],
            "parts": [
                {"mimeType": "multipart/alternative", "parts": [
                    {"mimeType": "text/plain", "body": {"data": b64("plain body")}},
                    {"mimeType": "text/html", "body": {"data": b64("<p>html body</p>")}},
                ]},
                {"mimeType": "text/plain", "filename": "notes.txt", "body": {"size": 12, "attachmentId": "a1"}},
            ],
        });
        let rendered = render_message(&json!({"id": "m1", "payload": payload}), Some(100));
        assert_eq!(rendered["subject"], "FedRAMP update");
        assert_eq!(rendered["body"], "plain body");
        assert_eq!(rendered["attachments"][0]["filename"], "notes.txt");
    }

    #[test]
    fn falls_back_to_html_rendered_as_text() {
        let html = "<html><head><style>p{}</style></head><body><p>Hello&nbsp;there</p>\
                    <script>alert(1)</script><div>Ana &amp; Bo &#8212; ok</div></body></html>";
        let payload = json!({"mimeType": "text/html", "body": {"data": b64(html)}});
        assert_eq!(
            extract_body(&payload).unwrap(),
            "Hello there\n\nAna & Bo \u{2014} ok"
        );
    }

    #[test]
    fn truncates_on_a_char_boundary() {
        assert_eq!(truncate("héllo", 2), ("hé".to_string(), true));
        assert_eq!(truncate("hé", 2), ("hé".to_string(), false));
    }
}
