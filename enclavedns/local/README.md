# EnclaveDNS Local

Opens `.eth` sites in an ordinary browser, through the IPFS, Swarm, Arweave
and Tor software on your own machine, and over HTTPS.

A browser needs an IP address for `playhh.eth`, but most ENS names point at
content instead (an IPFS or Swarm hash, an Arweave transaction, a Tor onion
service), so a browser alone shows "This site can't be reached". EnclaveDNS
Local is a small daemon on its own loopback address (`127.0.0.3`) that
closes that gap:

- **DNS for `.eth` and `.ens`.** Your DNS proxy forwards those names here; it
  asks [EnclaveDNS](../README.md) over DoH. Records a name stores on-chain come
  back unchanged. A name that exists but holds no address records gets
  `127.0.0.3`, so the browser comes here. Nonexistent names stay NXDOMAIN.
- **The site, from local software.** It reads what the name holds from
  EnclaveDNS (`/api/ens/<name>`) and serves it through:

  | The name holds | Served through | Default address |
  | --- | --- | --- |
  | IPFS / IPNS | kubo's gateway (content checked against its hash) | `http://127.0.0.1:8080` |
  | Swarm | a Bee node's API | `http://127.0.0.1:1633` |
  | Arweave | an AR.IO gateway | `http://127.0.0.1:3000` |
  | Tor onion | Tor's SOCKS port | `127.0.0.1:9050` |
  | a `url` text record | a redirect to it | |
  | only an address | a page showing what the name holds | |

  When that software is not running, the page says which protocol the site
  uses and how to start it, instead of the browser's generic error. It never
  falls back to a public gateway.
- **HTTPS.** No public CA can issue a certificate for an ENS name, so the
  daemon makes its own CA on first start and signs a certificate per name on
  demand. The CA carries an X.509 name constraint (`DNS:eth`, `DNS:ens`): even
  its own key cannot make a certificate a browser accepts for any other name.
  The key stays in the service's private state directory (mode 0700, its own
  user). Many dapps insist on HTTPS (some redirect themselves to it), and Web
  Crypto, service workers and wallets need a secure context.

IPNS names are resolved through kubo's API first: kubo's gateway waits for
many copies of an IPNS record (often its whole 30 s timeout), its API takes
the first valid one, and the answer then sits in kubo's cache for the gateway.

## Install (Arch Linux, systemd)

Needs the EnclaveDNS resolver setup first (dnscrypt-proxy on `127.0.0.1`,
Quad9 fallback on `127.0.0.2`), and kubo for IPFS sites
(`systemctl --user enable --now ipfs`).

```sh
cargo build --release
sudo bash install/install.sh      # reads your kubo's addresses from ~/.ipfs/config
```

It installs the binary to `/usr/local/bin`, the socket and service units
(systemd holds ports 80, 443 and 53 on `127.0.0.3` and starts the service on
demand), trusts the CA system-wide (`update-ca-trust`; Chromium and Firefox
read it through p11-kit), and forwards `eth` and `ens` from both DNS proxies.
Restart the browser afterwards so it loads the CA.

Undo everything, CA included: `sudo bash /etc/dnscrypt-proxy/undo-enclavedns-local.sh`.

`GET http://127.0.0.3/.well-known/enclavedns-local` (or `http://127.0.0.3/`)
shows which local software it detects.

## Options

```
enclavedns-local [--enclavedns URL] [--listen IP] [--ipfs URL] [--ipfs-api URL]
                 [--bee URL] [--arweave URL] [--tor HOST:PORT] [--state DIR]
                 [--redirect-https] [--http-port N --https-port N --dns-port N]
```

Without socket activation it binds the ports itself; `--http-port`,
`--https-port` and `--dns-port` let it run unprivileged for testing.

## Limits

- Chromium refuses to look up `.onion` names typed directly; a `.eth` name
  that points at an onion service works through this.
- Arweave and Swarm content is served as the local node returns it; kubo is
  the only backend that checks content against its address itself.
- `.eth` answers carry no DNSSEC: ENS is not DNS. EnclaveDNS reads ENS from
  several Ethereum providers at once and answers only when they agree.
