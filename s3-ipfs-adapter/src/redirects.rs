//! The IPFS `_redirects` file (specs.ipfs.tech/http-gateways/web-redirects-file).
//!
//! A gateway serving a website root evaluates these rules ONLY for a request
//! path that does not resolve in the DAG, so shipped files always win. We
//! support the shapes a static site actually uses:
//!
//!   /from            /to.html        200   # rewrite: serve /to.html's bytes
//!   /old             /new            301   # redirect (default status if absent)
//!   /prefix/*        /to             200   # splat prefix match
//!   /*               /404.html       404   # catch-all: branded 404 page
//!
//! First matching rule wins. A `:splat` token in the target is replaced with
//! the `/*` remainder. Placeholder params (`/:name`) and forced (`!`) rules are
//! not implemented — no site here uses them; unknown lines are skipped.

/// One parsed rewrite/redirect rule.
struct Rule {
    from: String,
    to: String,
    status: u16,
}

/// The compiled `_redirects` file. Cheap to keep behind an `Rc` per root CID
/// (immutable content ⇒ the parse is valid for that root forever).
pub struct Redirects {
    rules: Vec<Rule>,
}

/// A matched rule resolved against a concrete request path.
pub struct Match {
    pub to: String,
    pub status: u16,
}

/// Parse a `_redirects` file. Blank lines and `#` comments are ignored; a line
/// is `from to [status]` (status defaults to 301, per the spec).
pub fn parse(text: &str) -> Redirects {
    let mut rules = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut it = line.split_whitespace();
        let (Some(from), Some(to)) = (it.next(), it.next()) else {
            continue; // a lone token is not a rule
        };
        let status = it.next().and_then(|s| s.parse::<u16>().ok()).unwrap_or(301);
        rules.push(Rule { from: from.to_string(), to: to.to_string(), status });
    }
    Redirects { rules }
}

impl Redirects {
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// First rule whose `from` matches `path` (an absolute request path like
    /// `/develop`), with any `:splat` substituted into the target.
    pub fn lookup(&self, path: &str) -> Option<Match> {
        for r in &self.rules {
            if let Some(splat) = match_from(&r.from, path) {
                let to = if r.to.contains(":splat") {
                    r.to.replace(":splat", &splat)
                } else {
                    r.to.clone()
                };
                return Some(Match { to, status: r.status });
            }
        }
        None
    }
}

/// Match `from` against `path`. Returns the splat remainder on success (empty
/// for an exact match). A `from` ending in `/*` is a prefix match; `/*` alone
/// matches everything.
fn match_from(from: &str, path: &str) -> Option<String> {
    if let Some(prefix) = from.strip_suffix("/*") {
        if prefix.is_empty() {
            return Some(path.trim_start_matches('/').to_string());
        }
        if path == prefix {
            return Some(String::new());
        }
        return path
            .strip_prefix(prefix)
            .filter(|rest| rest.starts_with('/'))
            .map(|rest| rest[1..].to_string());
    }
    (from == path).then(String::new)
}

#[cfg(test)]
mod tests {
    use super::*;

    // The live enclave.host site's _redirects, verbatim (comments + rules).
    const SITE: &str = "\
# Pretty URLs
/apps       /apps.html       200
/develop    /develop.html    200
/dashboard  /dashboard.html  200
/host       /host.html       200
/admin      /admin.html      200
/terms      /terms.html      200
/privacy    /privacy.html    200
/sso/authorize /sso/authorize.html 200
/apps/deploy   /apps/deploy.html   200
/apps/publish  /apps/publish.html  200
/deploy        /apps.html          200
/publish       /apps.html          200
# catch-all
/* /404.html 404
";

    #[test]
    fn exact_rewrites_win_and_carry_status() {
        let r = parse(SITE);
        for (path, to) in [
            ("/develop", "/develop.html"),
            ("/dashboard", "/dashboard.html"),
            ("/sso/authorize", "/sso/authorize.html"),
            ("/apps/deploy", "/apps/deploy.html"),
            ("/deploy", "/apps.html"),
            ("/publish", "/apps.html"),
        ] {
            let m = r.lookup(path).unwrap_or_else(|| panic!("no match for {path}"));
            assert_eq!(m.to, to, "{path}");
            assert_eq!(m.status, 200, "{path}");
        }
    }

    #[test]
    fn catch_all_serves_branded_404() {
        let r = parse(SITE);
        for path in ["/nope", "/a/b/c", "/random-xyz"] {
            let m = r.lookup(path).unwrap();
            assert_eq!(m.to, "/404.html");
            assert_eq!(m.status, 404);
        }
    }

    #[test]
    fn first_match_wins_over_catch_all() {
        // /develop must resolve to develop.html (200), never the /* 404.
        let r = parse(SITE);
        let m = r.lookup("/develop").unwrap();
        assert_eq!((m.to.as_str(), m.status), ("/develop.html", 200));
    }

    #[test]
    fn comments_and_blanks_ignored() {
        let r = parse("\n# just a comment\n   \n/a /b 200\n");
        assert_eq!(r.lookup("/a").unwrap().to, "/b");
        assert!(r.lookup("/nope").is_none());
    }

    #[test]
    fn status_defaults_to_301_when_absent() {
        let r = parse("/old /new");
        let m = r.lookup("/old").unwrap();
        assert_eq!(m.status, 301);
    }

    #[test]
    fn splat_prefix_and_substitution() {
        let r = parse("/docs/* /help/:splat 200\n");
        let m = r.lookup("/docs/a/b").unwrap();
        assert_eq!(m.to, "/help/a/b");
        assert_eq!(m.status, 200);
        // the prefix boundary is a slash: /docsX must NOT match /docs/*
        assert!(r.lookup("/docsX").is_none());
    }

    #[test]
    fn empty_file_matches_nothing() {
        let r = parse("# nothing but comments\n");
        assert!(r.is_empty());
        assert!(r.lookup("/anything").is_none());
    }
}
