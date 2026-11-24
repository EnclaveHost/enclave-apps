//! Read existing, authenticated objects without constructing an indexer client.
use crate::hosts::Hosts;
use crate::rhp4::{Client, default_client};
use crate::{AppKey, Download, DownloadError, DownloadOptions, Host, Object};
use std::sync::Arc;

/// Direct host reader using an operator-pinned host list and an existing app key.
/// It cannot upload, renew contracts, refill accounts, or discover new hosts.
/// Reads consume the app's existing prepaid host account balances.
pub struct RecoveryReader {
    hosts: Hosts,
    key: Arc<AppKey>,
}
impl RecoveryReader {
    pub fn new(key: AppKey, hosts: Vec<Host>) -> Self {
        Self::with_transport(key, hosts, default_client())
    }
    pub(crate) fn with_transport(key: AppKey, hosts: Vec<Host>, transport: Client) -> Self {
        let known = Hosts::new(transport);
        known.update(hosts, true);
        Self {
            hosts: known,
            key: Arc::new(key),
        }
    }
    pub fn download(
        &self,
        object: &Object,
        options: DownloadOptions,
    ) -> Result<Download, DownloadError> {
        Download::new(object, self.hosts.clone(), self.key.clone(), options)
    }
}
