//! A small multipart/form-data parser, because that is how every OpenAI SDK
//! ships an audio file. Boundary handling per RFC 7578: parts delimited by
//! CRLF--boundary, headers to the first blank line, the rest is the body. No
//! nested multiparts, no content-transfer-encoding (nothing sends it), and a
//! hard part-count cap - this parses ONE form with one file and a few string
//! fields, not arbitrary mail.

pub struct Part {
    pub name: String,
    pub filename: Option<String>,
    pub data: Vec<u8>,
}

/// Extract the boundary from a Content-Type header value.
pub fn boundary(content_type: &str) -> Option<String> {
    let (kind, rest) = content_type.split_once(';')?;
    if !kind.trim().eq_ignore_ascii_case("multipart/form-data") {
        return None;
    }
    for param in rest.split(';') {
        let (k, v) = match param.split_once('=') {
            Some(kv) => kv,
            None => continue,
        };
        if k.trim().eq_ignore_ascii_case("boundary") {
            let v = v.trim();
            let v = v.strip_prefix('"').and_then(|s| s.strip_suffix('"')).unwrap_or(v);
            if !v.is_empty() {
                return Some(v.to_string());
            }
        }
    }
    None
}

pub fn parse(body: &[u8], boundary: &str) -> Result<Vec<Part>, String> {
    let delim: Vec<u8> = format!("--{boundary}").into_bytes();
    let mut parts = Vec::new();
    // the first delimiter may or may not be preceded by CRLF
    let mut pos = find(body, &delim, 0).ok_or("multipart: boundary never appears")?;
    pos += delim.len();
    loop {
        if body[pos..].starts_with(b"--") {
            break; // closing delimiter
        }
        // skip the CRLF after the delimiter
        if body[pos..].starts_with(b"\r\n") {
            pos += 2;
        }
        let head_end = find(body, b"\r\n\r\n", pos).ok_or("multipart: part without header end")?;
        let headers = String::from_utf8_lossy(&body[pos..head_end]);
        let (mut name, mut filename) = (None, None);
        for line in headers.split("\r\n") {
            let Some((k, v)) = line.split_once(':') else { continue };
            if !k.trim().eq_ignore_ascii_case("content-disposition") {
                continue;
            }
            for param in v.split(';') {
                let Some((pk, pv)) = param.split_once('=') else { continue };
                let pv = pv.trim();
                let pv = pv.strip_prefix('"').and_then(|s| s.strip_suffix('"')).unwrap_or(pv);
                match pk.trim().to_ascii_lowercase().as_str() {
                    "name" => name = Some(pv.to_string()),
                    "filename" => filename = Some(pv.to_string()),
                    _ => {}
                }
            }
        }
        let body_start = head_end + 4;
        // the part body runs to CRLF + delimiter (the CRLF is part of the
        // delimiter per RFC 7578, which is what lets a body carry "--{b}")
        let crlf_delim: Vec<u8> = format!("\r\n--{boundary}").into_bytes();
        let body_end = find(body, &crlf_delim, body_start)
            .ok_or("multipart: unterminated part (no closing boundary)")?;
        let next = body_end + crlf_delim.len();
        parts.push(Part {
            name: name.ok_or("multipart: part without a name")?,
            filename,
            data: body[body_start..body_end].to_vec(),
        });
        if parts.len() > 16 {
            return Err("multipart: too many parts".into());
        }
        pos = next;
    }
    Ok(parts)
}

fn find(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if from > hay.len() || needle.is_empty() {
        return None;
    }
    hay[from..].windows(needle.len()).position(|w| w == needle).map(|p| p + from)
}

/// The string value of a named field, when present and utf-8.
pub fn field<'a>(parts: &'a [Part], name: &str) -> Option<&'a str> {
    parts
        .iter()
        .find(|p| p.name == name && p.filename.is_none())
        .and_then(|p| std::str::from_utf8(&p.data).ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn openai_style_body(b: &str) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(
            format!(
                "--{b}\r\ncontent-disposition: form-data; name=\"model\"\r\n\r\nwhisper-1\r\n\
                 --{b}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.wav\"\r\n\
                 Content-Type: audio/wav\r\n\r\n"
            )
            .as_bytes(),
        );
        v.extend_from_slice(b"RIFF\x00\x01\r\n--binary\x00junk");
        v.extend_from_slice(format!("\r\n--{b}--\r\n").as_bytes());
        v
    }

    #[test]
    fn the_openai_shape_parses_with_binary_intact() {
        let b = "----FormBoundary7MA4YWxk";
        let parts = parse(&openai_style_body(b), b).unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(field(&parts, "model"), Some("whisper-1"));
        let file = parts.iter().find(|p| p.name == "file").unwrap();
        assert_eq!(file.filename.as_deref(), Some("a.wav"));
        // the CRLF-dash-dash inside the binary body did not truncate it
        assert_eq!(file.data, b"RIFF\x00\x01\r\n--binary\x00junk");
    }

    #[test]
    fn boundary_extraction_handles_quotes_and_case() {
        assert_eq!(
            boundary("multipart/form-data; boundary=----x12"),
            Some("----x12".into())
        );
        assert_eq!(
            boundary("Multipart/Form-Data; charset=utf-8; boundary=\"a b\""),
            Some("a b".into())
        );
        assert_eq!(boundary("application/json"), None);
        assert_eq!(boundary("multipart/form-data"), None);
    }

    #[test]
    fn malformed_bodies_fail_with_words_not_panics() {
        assert!(parse(b"no boundary here", "b").is_err());
        assert!(parse(b"--b\r\nno header end", "b").is_err());
        // a part with headers but no closing boundary
        assert!(parse(b"--b\r\nContent-Disposition: form-data; name=\"x\"\r\n\r\ndata", "b").is_err());
    }
}
