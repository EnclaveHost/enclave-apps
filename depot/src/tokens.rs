//! Access tokens minted at runtime (an admin's API call), kept as SHA-256
//! hashes in one sealed object (`<prefix>tokens`), so adding a CI key or a
//! collaborator does not need a new app config. Config users still work and
//! are checked first; a minted token carries its own read/write patterns
//! and optional expiry.

use crate::app::now;
use crate::config::{sha256, Who};
use crate::store::{Saved, Store};
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};

pub const PREFIX: &str = "dpt_";

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Token {
    pub id: String,
    /// hex sha256 of the token
    pub hash: String,
    pub user: String,
    #[serde(default)]
    pub admin: bool,
    #[serde(default)]
    pub read: Vec<String>,
    #[serde(default)]
    pub write: Vec<String>,
    pub created: u64,
    #[serde(default)]
    pub expires: Option<u64>,
    #[serde(default)]
    pub note: String,
    #[serde(default)]
    pub by: String,
}

#[derive(Serialize, Deserialize, Default, Clone)]
pub struct Book {
    pub v: u32,
    pub rev: u64,
    pub tokens: Vec<Token>,
    #[serde(default)]
    pub writer: String,
}

pub struct Tokens {
    pub book: Book,
    etag: Option<String>,
    checked: Option<Instant>,
}

const NAME: &str = "tokens";

impl Tokens {
    pub fn new() -> Tokens {
        Tokens {
            book: Book::default(),
            etag: None,
            checked: None,
        }
    }

    pub fn refresh(&mut self, store: &mut Store, force: bool) -> Result<(), String> {
        if !force
            && self
                .checked
                .is_some_and(|t| t.elapsed() < Duration::from_secs(5))
        {
            return Ok(());
        }
        let key = store.k_object(NAME);
        match store.get_raw_sealed(NAME, NAME)? {
            Some((b, t)) => {
                let book: Book =
                    serde_json::from_slice(&b).map_err(|e| format!("token book: {e}"))?;
                store.check_witness(&key, Some((book.rev, &book.writer)))?;
                if book.rev < self.book.rev {
                    return Err("storage served an older token book (rollback?)".into());
                }
                self.book = book;
                self.etag = Some(t);
            }
            None => {
                store.check_witness(&key, None)?;
                if self.etag.is_some() {
                    return Err("the token book disappeared from storage".into());
                }
            }
        }
        self.checked = Some(Instant::now());
        Ok(())
    }

    pub fn find(&self, token: &str) -> Option<&Token> {
        use subtle::ConstantTimeEq;
        if !token.starts_with(PREFIX) {
            return None;
        }
        let h = crate::git::hex(&sha256(token.as_bytes()));
        let t = now();
        let mut found = None;
        for x in &self.book.tokens {
            if bool::from(x.hash.as_bytes().ct_eq(h.as_bytes()))
                && x.expires.map_or(true, |e| t < e)
            {
                found = Some(x);
            }
        }
        found
    }

    fn save(&mut self, store: &mut Store, mut book: Book) -> Result<bool, String> {
        book.v = 1;
        book.rev += 1;
        book.writer = store.nonce();
        let b = serde_json::to_vec(&book).unwrap();
        match store.put_raw_sealed(NAME, NAME, &b, self.etag.as_deref(), &book.writer, book.rev)? {
            Saved::Ok(t) => {
                self.book = book;
                self.etag = Some(t);
                Ok(true)
            }
            Saved::Conflict => Ok(false),
        }
    }

    /// Mint a token; returns (the token, its record). The token is shown once.
    pub fn mint(&mut self, store: &mut Store, mut rec: Token) -> Result<(String, Token), String> {
        let secret = format!("{PREFIX}{}", crate::git::hex(&crate::seal::random::<24>()));
        rec.id = crate::git::hex(&crate::seal::random::<6>());
        rec.hash = crate::git::hex(&sha256(secret.as_bytes()));
        rec.created = now();
        for _ in 0..8 {
            self.refresh(store, true)?;
            // a write that landed and was then built upon still counts
            if self.book.tokens.iter().any(|t| t.hash == rec.hash) {
                return Ok((secret, rec));
            }
            if self.book.tokens.len() >= 1000 {
                return Err("token book is full (1000)".into());
            }
            let mut b = self.book.clone();
            b.tokens.push(rec.clone());
            if self.save(store, b)? {
                return Ok((secret, rec));
            }
        }
        Err("token book busy; try again".into())
    }

    pub fn revoke(&mut self, store: &mut Store, id: &str) -> Result<bool, String> {
        for attempt in 0..8 {
            self.refresh(store, true)?;
            if !self.book.tokens.iter().any(|t| t.id == id) {
                return Ok(attempt > 0);
            }
            let mut b = self.book.clone();
            b.tokens.retain(|t| t.id != id);
            if self.save(store, b)? {
                return Ok(true);
            }
        }
        Err("token book busy; try again".into())
    }
}

/// A minted token's identity: `user` with the token's own permissions.
pub fn who(t: &Token) -> Who {
    Who::Token(Box::new(t.clone()))
}
