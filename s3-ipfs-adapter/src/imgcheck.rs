//! /add-image validation, the Rust port of the nan gateway's raster sniff +
//! strict SVG validator (ipfs-add-gateway.py: raster_kind, svg_error,
//! image_error). Policy is unchanged: VALIDATE AND REJECT, never
//! sanitize-and-rewrite (rewriters get beaten by parser differentials; a
//! refusal can't). The XML parser here is deliberately a strict SUBSET of
//! XML — anything it does not perfectly understand is a refusal, which can
//! only ever turn a legitimate image away, never let a hostile one through.
//!
//! Layers behind this validator (same as before the S3 move):
//!   - the site renders media only as CSS background-image / <img>, contexts
//!     where browsers never execute SVG scripts or load external resources;
//!   - the gateway serves /ipfs/* with `Content-Security-Policy: sandbox` +
//!     X-Content-Type-Options, so a direct navigation can't run script;
//!   - the store is the configured bucket: only bytes an upload route
//!     admitted are servable (the NoFetch equivalence).

const SVG_NS: &str = "http://www.w3.org/2000/svg";

/// script-capable or embedding elements an app image never needs
const BAD_ELEMENTS: [&str; 9] = [
    "script", "foreignobject", "iframe", "embed", "object",
    "audio", "video", "handler", "listener",
];
const BAD_ELEMENTS_EXTRA: &str = "annotation-xml";

pub fn raster_kind(b: &[u8]) -> Option<&'static str> {
    if b.len() < 12 {
        return None;
    }
    if b[0..8] == *b"\x89PNG\r\n\x1a\n" {
        return Some("png");
    }
    if b[0..3] == [0xff, 0xd8, 0xff] {
        return Some("jpeg");
    }
    if &b[0..6] == b"GIF87a" || &b[0..6] == b"GIF89a" {
        return Some("gif");
    }
    if &b[0..4] == b"RIFF" && &b[8..12] == b"WEBP" {
        return Some("webp");
    }
    None
}

/// (kind, error): kind "png"/"jpeg"/"gif"/"webp"/"svg" when accepted.
pub fn image_error(b: &[u8]) -> (Option<&'static str>, Option<String>) {
    if let Some(kind) = raster_kind(b) {
        return (Some(kind), None);
    }
    let head: Vec<u8> = {
        let h = &b[..b.len().min(512)];
        let h = h.strip_prefix(b"\xef\xbb\xbf".as_slice()).unwrap_or(h);
        let start = h
            .iter()
            .position(|&c| !matches!(c, b' ' | b'\t' | b'\r' | b'\n'))
            .unwrap_or(h.len());
        h[start..].to_ascii_lowercase()
    };
    if head.starts_with(b"<?xml") || head.starts_with(b"<svg") || contains(&head, b"<svg") {
        return match svg_error(b) {
            None => (Some("svg"), None),
            Some(e) => (None, Some(e)),
        };
    }
    (None, Some("unsupported image type - use PNG, JPEG, WebP, GIF, or SVG".into()))
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

/// strip the chars browsers ignore inside URLs before scheme checks
fn squeeze(v: &str) -> String {
    v.chars()
        .filter(|&c| c as u32 > 0x20)
        .flat_map(char::to_lowercase)
        .collect()
}

fn is_data_raster(sval: &str) -> bool {
    for mime in ["png", "jpg", "jpeg", "gif", "webp"] {
        if sval
            .strip_prefix("data:image/")
            .and_then(|r| r.strip_prefix(mime))
            .and_then(|r| r.strip_prefix(";base64,"))
            .is_some()
        {
            return true;
        }
    }
    false
}

/// CSS can't execute script, but url() pulls external resources on direct
/// navigation - allow only internal url(#...) targets, no @import.
fn css_error(css: &str) -> Option<String> {
    let low = squeeze(css);
    if low.contains("@import") {
        return Some("SVG styles must not use @import".into());
    }
    let mut rest = low.as_str();
    while let Some(p) = rest.find("url(") {
        let after = &rest[p + 4..];
        let target = after.trim_start_matches(['\'', '"']);
        if !target.starts_with('#') {
            return Some("SVG styles may only reference internal url(#...) targets".into());
        }
        rest = after;
    }
    None
}

/// Return an error string unless the bytes are a safe standalone SVG.
pub fn svg_error(b: &[u8]) -> Option<String> {
    let Ok(text) = std::str::from_utf8(b) else {
        return Some("SVG must be UTF-8".into());
    };
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let low = text.to_lowercase();
    // DTD machinery enables entity expansion tricks; no image needs it
    if low.contains("<!doctype") || low.contains("<!entity") {
        return Some("SVG must not contain DOCTYPE or entity declarations".into());
    }
    // processing instructions: only the leading <?xml declaration is allowed
    // (<?xml-stylesheet?> attaches external CSS on direct navigation)
    let mut from = 0;
    while let Some(p) = text[from..].find("<?") {
        let p = from + p;
        if p == 0 && is_xml_decl(text) {
            from = p + 2;
            continue;
        }
        return Some("SVG must not contain processing instructions".into());
    }
    let root = match parse_xml(text) {
        Ok(el) => el,
        Err(e) => return Some(format!("SVG is not well-formed XML: {e}")),
    };
    if !(root.ns.as_deref() == Some(SVG_NS) && root.local == "svg") {
        return Some("the root element must be <svg> in the SVG namespace".into());
    }
    check_element(&root)
}

fn is_xml_decl(text: &str) -> bool {
    text.strip_prefix("<?xml")
        .and_then(|r| r.chars().next())
        .is_some_and(|c| c.is_ascii_whitespace() || c == '?')
}

fn check_element(el: &Element) -> Option<String> {
    if el.ns.as_deref() != Some(SVG_NS) {
        return Some("SVG must not embed non-SVG-namespace elements".into());
    }
    let local = el.local.to_lowercase();
    if BAD_ELEMENTS.contains(&local.as_str()) || local == BAD_ELEMENTS_EXTRA {
        return Some(format!("SVG must not contain <{local}> elements"));
    }
    for (name, val) in &el.attrs {
        let lname = name.rsplit(':').next().unwrap_or(name).to_lowercase();
        if lname.starts_with("on") {
            let short: String = lname.chars().take(32).collect();
            return Some(format!("SVG must not carry event-handler attributes ({short})"));
        }
        let sval = squeeze(val);
        // scheme check on EVERY value: catches animated/indirect targets too
        // (values arrive entity-DECODED here, so &#106;avascript tricks are
        // already unfolded)
        if sval.contains("javascript:") || sval.contains("vbscript:") {
            return Some("SVG must not reference script URLs".into());
        }
        if lname == "href" && !(sval.starts_with('#') || is_data_raster(&sval)) {
            return Some(
                "SVG references must be internal (#id) or embedded raster data: URIs".into(),
            );
        }
        if lname == "attributename" {
            // Animating href re-points a link; animating an on* handler
            // (<set attributeName="onclick" to="…"/>) is a known SVG XSS
            // vector the element/attribute scan misses, because the dangerous
            // name rides as a VALUE here rather than as an attribute.
            if sval == "href" || sval == "xlink:href" || sval.starts_with("on") {
                return Some("SVG must not animate href or event-handler attributes".into());
            }
        }
        // url() is not only a CSS thing: fill/stroke/filter/mask/clip-path/
        // marker-* take a funciri too, and `fill="url(https://evil/x)"` is an
        // external reference this validator is supposed to refuse - it would
        // beacon the viewer's IP on a direct /ipfs/<cid> navigation, where the
        // sandbox CSP stops script but not subresource loads. Internal
        // url(#id) - the common gradient/filter case - stays fine.
        if lname == "style" || sval.contains("url(") {
            if let Some(err) = css_error(val) {
                return Some(err);
            }
        }
    }
    if el.local.to_lowercase() == "style" {
        let mut css = String::new();
        el.collect_text(&mut css);
        if let Some(err) = css_error(&css) {
            return Some(err);
        }
    }
    for child in &el.children {
        if let Node::Element(sub) = child {
            if let Some(err) = check_element(sub) {
                return Some(err);
            }
        }
    }
    None
}

// ---- a strict subset-of-XML parser ------------------------------------------
//
// Namespace-aware, entity-decoding, fail-closed: comments and CDATA are
// understood; DOCTYPE/ENTITY/PIs never reach here (pre-banned above);
// anything else unexpected is an error. ASCII element/attribute names only —
// stricter than the XML spec, which can only refuse, never admit.

struct Element {
    ns: Option<String>,
    local: String,
    attrs: Vec<(String, String)>, // qname (prefix kept), decoded value
    children: Vec<Node>,
}

enum Node {
    Element(Element),
    Text(String),
}

impl Element {
    fn collect_text(&self, out: &mut String) {
        for c in &self.children {
            match c {
                Node::Text(t) => out.push_str(t),
                Node::Element(e) => e.collect_text(out),
            }
        }
    }
}

struct Parser<'a> {
    s: &'a [u8],
    pos: usize,
}

type PResult<T> = Result<T, String>;

fn parse_xml(text: &str) -> PResult<Element> {
    let mut p = Parser { s: text.as_bytes(), pos: 0 };
    if is_xml_decl(text) {
        let end = text.find("?>").ok_or("unterminated xml declaration")?;
        p.pos = end + 2;
    }
    p.skip_misc()?;
    let (root, ns_ok) = p.parse_element(&NsScope::root())?;
    if !ns_ok {
        return Err("unbound namespace prefix".into());
    }
    p.skip_misc()?;
    if p.pos != p.s.len() {
        return Err("content after the root element".into());
    }
    Ok(root)
}

/// Prefix -> namespace bindings, scoped. Kept as a linked stack of frames.
struct NsScope<'a> {
    parent: Option<&'a NsScope<'a>>,
    default_ns: Option<Option<String>>, // Some(binding); None = inherit
    bindings: Vec<(String, String)>,
}

impl<'a> NsScope<'a> {
    fn root() -> NsScope<'static> {
        NsScope { parent: None, default_ns: Some(None), bindings: Vec::new() }
    }
    fn default(&self) -> Option<String> {
        match &self.default_ns {
            Some(d) => d.clone(),
            None => self.parent.and_then(|p| p.default()),
        }
    }
    fn lookup(&self, prefix: &str) -> Option<String> {
        if prefix == "xml" {
            return Some("http://www.w3.org/XML/1998/namespace".into());
        }
        for (p, uri) in &self.bindings {
            if p == prefix {
                return Some(uri.clone());
            }
        }
        self.parent.and_then(|p| p.lookup(prefix))
    }
}

impl<'a> Parser<'a> {
    fn ws(&mut self) {
        while self.pos < self.s.len() && self.s[self.pos].is_ascii_whitespace() {
            self.pos += 1;
        }
    }

    /// Skip whitespace and comments between top-level constructs.
    fn skip_misc(&mut self) -> PResult<()> {
        loop {
            self.ws();
            if self.s[self.pos..].starts_with(b"<!--") {
                self.skip_comment()?;
            } else {
                return Ok(());
            }
        }
    }

    fn skip_comment(&mut self) -> PResult<()> {
        let rest = &self.s[self.pos + 4..];
        let end = find_sub(rest, b"-->").ok_or("unterminated comment")?;
        self.pos += 4 + end + 3;
        Ok(())
    }

    fn name(&mut self) -> PResult<String> {
        let start = self.pos;
        while self.pos < self.s.len() {
            let b = self.s[self.pos];
            let ok = b.is_ascii_alphanumeric() || matches!(b, b'_' | b':' | b'-' | b'.');
            let first_ok = b.is_ascii_alphabetic() || matches!(b, b'_' | b':');
            if self.pos == start {
                if !first_ok {
                    return Err("bad name".into());
                }
            } else if !ok {
                break;
            }
            self.pos += 1;
        }
        if self.pos == start {
            return Err("bad name".into());
        }
        Ok(std::str::from_utf8(&self.s[start..self.pos]).unwrap().to_string())
    }

    /// Parse one element (cursor at '<'). Returns (element, namespaces_ok).
    fn parse_element(&mut self, scope: &NsScope) -> PResult<(Element, bool)> {
        if self.s.get(self.pos) != Some(&b'<') {
            return Err("expected an element".into());
        }
        self.pos += 1;
        let qname = self.name()?;
        let mut attrs: Vec<(String, String)> = Vec::new();
        let mut bindings: Vec<(String, String)> = Vec::new();
        let mut default_ns: Option<Option<String>> = None;
        let self_closing;
        loop {
            self.ws();
            match self.s.get(self.pos) {
                Some(b'>') => {
                    self.pos += 1;
                    self_closing = false;
                    break;
                }
                Some(b'/') => {
                    if self.s.get(self.pos + 1) != Some(&b'>') {
                        return Err("bad tag end".into());
                    }
                    self.pos += 2;
                    self_closing = true;
                    break;
                }
                Some(_) => {
                    let aname = self.name()?;
                    self.ws();
                    if self.s.get(self.pos) != Some(&b'=') {
                        return Err(format!("attribute {aname} has no value"));
                    }
                    self.pos += 1;
                    self.ws();
                    let quote = *self.s.get(self.pos).ok_or("unterminated attribute")?;
                    if quote != b'"' && quote != b'\'' {
                        return Err("attribute value must be quoted".into());
                    }
                    self.pos += 1;
                    let vstart = self.pos;
                    while self.pos < self.s.len() && self.s[self.pos] != quote {
                        if self.s[self.pos] == b'<' {
                            return Err("'<' in attribute value".into());
                        }
                        self.pos += 1;
                    }
                    if self.pos >= self.s.len() {
                        return Err("unterminated attribute value".into());
                    }
                    let raw = std::str::from_utf8(&self.s[vstart..self.pos])
                        .map_err(|_| "bad utf-8")?;
                    self.pos += 1;
                    let value = decode_entities(raw)?;
                    if attrs.iter().any(|(n, _)| n == &aname) {
                        return Err(format!("duplicate attribute {aname}"));
                    }
                    if aname == "xmlns" {
                        default_ns = Some(if value.is_empty() { None } else { Some(value.clone()) });
                    } else if let Some(prefix) = aname.strip_prefix("xmlns:") {
                        bindings.push((prefix.to_string(), value.clone()));
                    }
                    attrs.push((aname, value));
                }
                None => return Err("unterminated tag".into()),
            }
        }
        let scope = NsScope { parent: Some(scope), default_ns, bindings };
        let (ns, local, mut ns_ok) = resolve(&qname, &scope)?;
        let mut el = Element { ns, local, attrs, children: Vec::new() };
        if self_closing {
            return Ok((el, ns_ok));
        }
        // Children until the matching close tag.
        loop {
            if self.pos >= self.s.len() {
                return Err(format!("unterminated <{qname}>"));
            }
            if self.s[self.pos] == b'<' {
                if self.s[self.pos..].starts_with(b"<!--") {
                    self.skip_comment()?;
                    continue;
                }
                if self.s[self.pos..].starts_with(b"<![CDATA[") {
                    let rest = &self.s[self.pos + 9..];
                    let end = find_sub(rest, b"]]>").ok_or("unterminated CDATA")?;
                    let t = std::str::from_utf8(&rest[..end]).map_err(|_| "bad utf-8")?;
                    el.children.push(Node::Text(t.to_string()));
                    self.pos += 9 + end + 3;
                    continue;
                }
                if self.s[self.pos..].starts_with(b"</") {
                    self.pos += 2;
                    let close = self.name()?;
                    if close != qname {
                        return Err(format!("mismatched </{close}> for <{qname}>"));
                    }
                    self.ws();
                    if self.s.get(self.pos) != Some(&b'>') {
                        return Err("bad close tag".into());
                    }
                    self.pos += 1;
                    return Ok((el, ns_ok));
                }
                if self.s[self.pos..].starts_with(b"<!") || self.s[self.pos..].starts_with(b"<?") {
                    // DOCTYPE/ENTITY/PIs are pre-banned; anything else here
                    // (<!ATTLIST, a stray declaration) is a refusal too.
                    return Err("unsupported markup declaration".into());
                }
                let (child, child_ok) = self.parse_element(&scope)?;
                ns_ok = ns_ok && child_ok;
                el.children.push(Node::Element(child));
                continue;
            }
            let start = self.pos;
            while self.pos < self.s.len() && self.s[self.pos] != b'<' {
                self.pos += 1;
            }
            let t = std::str::from_utf8(&self.s[start..self.pos]).map_err(|_| "bad utf-8")?;
            el.children.push(Node::Text(decode_entities(t)?));
        }
    }
}

/// (namespace, local, ok): resolve a qname against the scope. An unbound
/// prefix reports ok=false (the caller turns that into a refusal).
fn resolve(qname: &str, scope: &NsScope) -> PResult<(Option<String>, String, bool)> {
    match qname.split_once(':') {
        Some((prefix, local)) => {
            if prefix.is_empty() || local.is_empty() || local.contains(':') {
                return Err(format!("bad qualified name {qname}"));
            }
            match scope.lookup(prefix) {
                Some(uri) => Ok((Some(uri), local.to_string(), true)),
                None => Ok((None, local.to_string(), false)),
            }
        }
        None => Ok((scope.default(), qname.to_string(), true)),
    }
}

fn find_sub(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if hay.len() < needle.len() {
        return None;
    }
    (0..=hay.len() - needle.len()).find(|&i| &hay[i..i + needle.len()] == needle)
}

/// The five predefined entities plus numeric character references; anything
/// else (an undeclared entity, a bare '&') is a refusal, matching a strict
/// XML parser's ParseError.
fn decode_entities(s: &str) -> PResult<String> {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(a) = rest.find('&') {
        out.push_str(&rest[..a]);
        rest = &rest[a..];
        let semi = rest.find(';').ok_or("bad entity reference")?;
        let ent = &rest[1..semi];
        match ent {
            "amp" => out.push('&'),
            "lt" => out.push('<'),
            "gt" => out.push('>'),
            "quot" => out.push('"'),
            "apos" => out.push('\''),
            _ => {
                let code = ent
                    .strip_prefix("#x")
                    .or_else(|| ent.strip_prefix("#X"))
                    .and_then(|h| u32::from_str_radix(h, 16).ok())
                    .or_else(|| {
                        ent.strip_prefix('#')
                            .filter(|d| d.chars().all(|c| c.is_ascii_digit()))
                            .and_then(|d| d.parse().ok())
                    });
                match code.and_then(char::from_u32) {
                    Some(c) => out.push(c),
                    None => return Err(format!("undefined entity &{ent};",)),
                }
            }
        }
        rest = &rest[semi + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const OK_SVG: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 10 10">
      <defs><linearGradient id="g"><stop offset="0" stop-color="#fff"/></linearGradient></defs>
      <rect width="10" height="10" fill="url(#g)"/>
      <use href="#g"/>
      <image href="data:image/png;base64,iVBORw0KGgo="/>
      <style>.a { fill: url(#g); }</style>
      <text style="fill:url(#g)">hi &amp; bye</text>
    </svg>"##;

    fn err(svg: &str) -> String {
        svg_error(svg.as_bytes()).expect("expected a refusal")
    }

    #[test]
    fn clean_svg_passes() {
        assert_eq!(svg_error(OK_SVG.as_bytes()), None);
        let decl = format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n{OK_SVG}");
        assert_eq!(svg_error(decl.as_bytes()), None);
        let bom = format!("\u{feff}{decl}");
        assert_eq!(svg_error(bom.as_bytes()), None);
    }

    #[test]
    fn real_asset_passes() {
        for f in ["logo.svg", "banner.svg"] {
            let p = format!("{}/assets/{f}", env!("CARGO_MANIFEST_DIR"));
            let Ok(bytes) = std::fs::read(&p) else { continue };
            assert_eq!(svg_error(&bytes), None, "{f}");
        }
    }

    #[test]
    fn script_vectors_refused() {
        assert!(err(r##"<svg xmlns="http://www.w3.org/2000/svg"><script>1</script></svg>"##)
            .contains("<script>"));
        assert!(err(r##"<svg xmlns="http://www.w3.org/2000/svg" onload="x()"/>"##)
            .contains("event-handler"));
        assert!(err(r##"<svg xmlns="http://www.w3.org/2000/svg"><a href="javascript:1"/></svg>"##)
            .contains("script URLs"));
        // entity-encoded scheme unfolds before the check
        assert!(err(r##"<svg xmlns="http://www.w3.org/2000/svg"><a href="&#106;avascript:1"/></svg>"##)
            .contains("script URLs"));
        // whitespace/control smuggling squeezed out
        assert!(err("<svg xmlns=\"http://www.w3.org/2000/svg\"><a href=\"java\tscript:1\"/></svg>")
            .contains("script URLs"));
        assert!(err(r##"<svg xmlns="http://www.w3.org/2000/svg"><foreignObject/></svg>"##)
            .contains("<foreignobject>"));
    }

    #[test]
    fn external_reference_vectors_refused() {
        assert!(err(r##"<svg xmlns="http://www.w3.org/2000/svg"><image href="https://evil/x.png"/></svg>"##)
            .contains("internal (#id)"));
        assert!(err(r##"<svg xmlns="http://www.w3.org/2000/svg"><rect fill="url(https://evil/x)"/></svg>"##)
            .contains("url(#...)"));
        assert!(err(r##"<svg xmlns="http://www.w3.org/2000/svg"><style>@import url(https://evil/a.css);</style></svg>"##)
            .contains("@import"));
        assert!(err(r##"<svg xmlns="http://www.w3.org/2000/svg"><style>.a{background:url('https://evil/b')}</style></svg>"##)
            .contains("url(#...)"));
        assert!(err(r##"<svg xmlns="http://www.w3.org/2000/svg"><rect style="fill:url(//evil/x)"/></svg>"##)
            .contains("url(#...)"));
        // the scheme check runs on EVERY value, animation targets included
        assert!(err(r##"<svg xmlns="http://www.w3.org/2000/svg"><animate to="javascript:1"/></svg>"##)
            .contains("script URLs"));
        assert!(err(r##"<svg xmlns="http://www.w3.org/2000/svg"><animate attributeName="href" to="#x"/></svg>"##)
            .contains("animate href"));
        // animating an event-handler attribute is an XSS vector even though
        // `onclick` is a value here, not an attribute the element scan sees
        assert!(err(r##"<svg xmlns="http://www.w3.org/2000/svg"><set attributeName="onclick" to="alert(1)"/></svg>"##)
            .contains("event-handler"));
    }

    #[test]
    fn structure_vectors_refused() {
        assert!(err(r##"<!DOCTYPE svg [<!ENTITY x "y">]><svg xmlns="http://www.w3.org/2000/svg"/>"##)
            .contains("DOCTYPE"));
        assert!(err(r##"<?xml version="1.0"?><?xml-stylesheet href="e.css"?><svg xmlns="http://www.w3.org/2000/svg"/>"##)
            .contains("processing instructions"));
        assert!(err(r##"<svg xmlns="http://www.w3.org/2000/svg"><x xmlns="http://evil"/></svg>"##)
            .contains("non-SVG-namespace"));
        assert!(err(r##"<svg/>"##).contains("root element"));
        assert!(err(r##"<svg xmlns="http://www.w3.org/2000/svg"><undefined-entity>&foo;</undefined-entity></svg>"##)
            .contains("not well-formed"));
        assert!(err(r##"<svg xmlns="http://www.w3.org/2000/svg"><q:x/></svg>"##)
            .contains("not well-formed"));
        assert!(err("no xml at all").contains("not well-formed"));
        assert_eq!(svg_error(&[0xff, 0xfe, 0x00]), Some("SVG must be UTF-8".into()));
    }

    #[test]
    fn xlink_href_checked_by_local_name() {
        let s = r##"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink">
            <use xlink:href="https://evil/#x"/></svg>"##;
        assert!(err(s).contains("internal (#id)"));
        let ok = r##"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink">
            <use xlink:href="#x"/></svg>"##;
        assert_eq!(svg_error(ok.as_bytes()), None);
    }

    #[test]
    fn raster_magics() {
        let png = [0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n', 0, 0, 0, 0];
        assert_eq!(raster_kind(&png), Some("png"));
        let mut webp = *b"RIFF0000WEBPVP8 ";
        assert_eq!(raster_kind(&webp), Some("webp"));
        webp[8] = b'X';
        assert_eq!(raster_kind(&webp), None);
        let (kind, e) = image_error(&png);
        assert_eq!((kind, e), (Some("png"), None));
        let (kind, e) = image_error(b"plain text, certainly no image");
        assert!(kind.is_none() && e.unwrap().contains("unsupported image type"));
    }

    #[test]
    fn svg_detection_reaches_validator() {
        let (kind, e) = image_error(OK_SVG.as_bytes());
        assert_eq!((kind, e), (Some("svg"), None));
        let (kind, e) =
            image_error(br##"  <svg xmlns="http://www.w3.org/2000/svg" onclick="x"/>"##);
        assert!(kind.is_none() && e.unwrap().contains("event-handler"));
    }

    #[test]
    fn cdata_reaches_css_check() {
        let s = r##"<svg xmlns="http://www.w3.org/2000/svg"><style><![CDATA[.a{fill:url(https://e/x)}]]></style></svg>"##;
        assert!(err(s).contains("url(#...)"));
        let ok = r##"<svg xmlns="http://www.w3.org/2000/svg"><style><![CDATA[.a{fill:url(#g)}]]></style></svg>"##;
        assert_eq!(svg_error(ok.as_bytes()), None);
    }
}
