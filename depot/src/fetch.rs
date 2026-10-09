//! The response body of a fetch: upload-pack's negotiation lines, then the
//! pack generated from stored entries as the client drains it.

use crate::app::{App, RepoIo};
use crate::git::upload::Fetch;
use crate::serve::Source;

pub struct FetchSource {
    pub repo_id: String,
    pub repo: String,
    pub fetch: Fetch,
    pub started: std::time::Instant,
    pub bytes: u64,
}

impl Source<App> for FetchSource {
    fn pull(&mut self, app: &mut App, out: &mut Vec<u8>) -> Result<bool, String> {
        let App {
            store,
            repos,
            objects,
            cfg,
            stats,
            ..
        } = app;
        let r = repos
            .get(&self.repo_id)
            .ok_or("the repository was removed during the fetch")?;
        let mut io = RepoIo {
            store,
            repo: &self.repo_id,
            ix: &r.ix,
            cache: objects,
            max: cfg.max_object,
        };
        let before = out.len();
        let done = self.fetch.produce(&mut io, out, 256 << 10)?;
        self.bytes += (out.len() - before) as u64;
        stats.sent_bytes += (out.len() - before) as u64;
        if done && self.fetch.objects() > 0 {
            stats.fetches += 1;
            eprintln!(
                "[depot] fetch {}: {} objects, {:.1} MiB in {:.1}s",
                self.repo,
                self.fetch.objects(),
                self.bytes as f64 / 1048576.0,
                self.started.elapsed().as_secs_f64()
            );
        }
        Ok(done)
    }
}
