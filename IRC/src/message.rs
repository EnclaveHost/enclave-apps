//! RFC 1459/2812 client message parsing.
//!
//! `<line> ::= [':' <prefix> <SPACE>] <command> <params> <crlf>`
//! A prefix sent by a client is ignored (the server always knows who is
//! talking from the connection), and the trailing parameter (`" :..."`)
//! becomes the last entry of `params`.

pub struct Msg {
    pub cmd: String,
    pub params: Vec<String>,
}

pub fn parse(line: &str) -> Option<Msg> {
    let mut rest = line.trim_start();
    if rest.starts_with(':') {
        rest = rest[rest.find(' ')?..].trim_start();
    }
    if rest.is_empty() {
        return None;
    }
    let (head, trailing) = match rest.find(" :") {
        Some(i) => (&rest[..i], Some(rest[i + 2..].to_string())),
        None => (rest, None),
    };
    let mut words = head.split_ascii_whitespace();
    let cmd = words.next()?.to_ascii_uppercase();
    let mut params: Vec<String> = words.map(str::to_string).collect();
    if let Some(t) = trailing {
        params.push(t);
    }
    Some(Msg { cmd, params })
}

/// RFC "ascii" casemapping: nicknames and channel names compare
/// case-insensitively (we advertise `CASEMAPPING=ascii` in ISUPPORT).
pub fn lower(s: &str) -> String {
    s.to_ascii_lowercase()
}

pub fn valid_nick(s: &str) -> bool {
    if s.is_empty() || s.len() > super::NICK_MAX {
        return false;
    }
    let first = s.chars().next().unwrap();
    if first.is_ascii_digit() || first == '-' {
        return false;
    }
    s.chars().all(|c| {
        c.is_ascii_alphanumeric() || matches!(c, '[' | ']' | '\\' | '`' | '_' | '^' | '{' | '|' | '}' | '-')
    })
}

pub fn valid_channel(s: &str) -> bool {
    s.len() >= 2
        && s.len() <= super::CHANNEL_MAX
        && s.starts_with('#')
        && !s.contains([' ', ',', '\x07', ':'])
}
