//go:build linux

package main

import (
	"context"
	"encoding/json"
	"net/http"
	"sync"
	"time"
)

// One record per client certificate.
//
// Sunshine appends a record for every completed pairing and authorizes a
// client certificate only while exactly one record carries it. Moonlight
// keeps its certificate, so pairing one Moonlight a second time leaves that
// client refused on every HTTPS request ("The client is not authorized.
// Certificate verification failed."), and each further try adds a record.
//
// After Sunshine accepts a PIN this gateway therefore removes the older
// records of every certificate that has more than one. Which record carries
// which certificate comes from the desktop automation service (a digest per
// record of Sunshine's own state file); Sunshine's admin API names the
// records and removes them. When the digests cannot be had, only the newest
// record is kept: the client that just paired works, others pair again.
//
// The newest record of a certificate always stays, so the list never becomes
// empty here: Sunshine ends the running application when it does.
const (
	sunshineClientsURL = "https://" + sunshineAPIAddr + "/api/clients/list"
	sunshineUnpairURL  = "https://" + sunshineAPIAddr + "/api/clients/unpair"

	maxClientList   = 256 << 10
	maxClients      = 512
	maxClientUUID   = 64
	maxUnpairReply  = 4096
	maxDigestReply  = 256 << 10
	tidyTimeout     = 10 * time.Second
	digestTries     = 3
	digestRetryWait = 200 * time.Millisecond

	// A client's first request with its certificate goes to this port.
	gatedTunnelPort = "47984"
)

func validClientUUID(s string) bool {
	if len(s) < 1 || len(s) > maxClientUUID {
		return false
	}
	for i := 0; i < len(s); i++ {
		c := s[i]
		if !(c >= '0' && c <= '9' || c >= 'a' && c <= 'f' || c >= 'A' && c <= 'F' || c == '-') {
			return false
		}
	}
	return true
}

// clientUUIDs returns Sunshine's paired records in stored order, oldest
// first. Every record must carry a distinct well-formed identifier.
func clientUUIDs(b []byte) ([]string, bool) {
	var list struct {
		Certs *[]struct {
			UUID *string `json:"uuid"`
		} `json:"named_certs"`
	}
	if !decodeOne(b, &list) || list.Certs == nil || len(*list.Certs) > maxClients {
		return nil, false
	}
	out := make([]string, 0, len(*list.Certs))
	seen := make(map[string]bool, len(*list.Certs))
	for _, c := range *list.Certs {
		if c.UUID == nil || !validClientUUID(*c.UUID) || seen[*c.UUID] {
			return nil, false
		}
		seen[*c.UUID] = true
		out = append(out, *c.UUID)
	}
	return out, true
}

func validDigest(s string) bool {
	if len(s) != 64 {
		return false
	}
	for i := 0; i < len(s); i++ {
		if c := s[i]; !(c >= '0' && c <= '9' || c >= 'a' && c <= 'f') {
			return false
		}
	}
	return true
}

// recordDigests decodes the automation service's answer: a certificate
// digest per record identifier.
func recordDigests(b []byte) (map[string]string, bool) {
	var reply struct {
		OK      bool `json:"ok"`
		Clients *[]struct {
			UUID *string `json:"uuid"`
			Cert *string `json:"cert_sha256"`
		} `json:"clients"`
	}
	if !decodeOne(b, &reply) || !reply.OK || reply.Clients == nil || len(*reply.Clients) > maxClients {
		return nil, false
	}
	out := make(map[string]string, len(*reply.Clients))
	for _, c := range *reply.Clients {
		if c.UUID == nil || c.Cert == nil || !validClientUUID(*c.UUID) || !validDigest(*c.Cert) {
			return nil, false
		}
		if _, dup := out[*c.UUID]; dup {
			return nil, false
		}
		out[*c.UUID] = *c.Cert
	}
	return out, true
}

// surplus names the records to remove, given the records in stored order.
// With a digest for every record: each record that a later one repeats.
// Otherwise: every record but the newest. The newest record of a certificate
// is never named, so at least one record always remains.
func surplus(records []string, digests map[string]string) []string {
	known := digests != nil
	for _, id := range records {
		if _, ok := digests[id]; !ok {
			known = false
		}
	}
	var out []string
	if !known {
		if len(records) > 1 {
			out = append(out, records[:len(records)-1]...)
		}
		return out
	}
	last := make(map[string]int, len(records))
	for i, id := range records {
		last[digests[id]] = i
	}
	for i, id := range records {
		if last[digests[id]] != i {
			out = append(out, id)
		}
	}
	return out
}

func (g *gateway) listClients(ctx context.Context, cred []byte) ([]string, int) {
	b, code := g.sunshineCall(ctx, http.MethodGet, sunshineClientsURL, nil, cred, maxClientList)
	if code != 0 {
		return nil, code
	}
	records, ok := clientUUIDs(b)
	if !ok {
		return nil, http.StatusBadGateway
	}
	return records, 0
}

// digestsFor asks the desktop which certificate each record carries. The
// answer is used only when it covers every record Sunshine lists; a state
// file caught between two writes gets a few more tries.
func (g *gateway) digestsFor(ctx context.Context, records []string) map[string]string {
	for try := 0; try < digestTries; try++ {
		if try > 0 {
			select {
			case <-ctx.Done():
				return nil
			case <-time.After(g.cfg.digestRetry):
			}
		}
		deadline, ok := ctx.Deadline()
		if !ok {
			deadline = time.Now().Add(g.cfg.tidyTimeout)
		}
		status, b, err := g.automationExchange(ctx, "/paired", []byte("{}"), deadline)
		if err != nil || status != http.StatusOK || len(b) > maxDigestReply {
			continue
		}
		digests, ok := recordDigests(b)
		if !ok {
			continue
		}
		covered := true
		for _, id := range records {
			if _, ok := digests[id]; !ok {
				covered = false
			}
		}
		if covered {
			return digests
		}
	}
	return nil
}

type unpairRequest struct {
	UUID string `json:"uuid"`
}

// tidyClients leaves one record per client certificate. It returns zero when
// Sunshine's list is in that state afterwards, or a status for the client.
func (g *gateway) tidyClients(ctx context.Context, cred []byte) int {
	records, code := g.listClients(ctx, cred)
	if code != 0 {
		return code
	}
	if len(records) == 0 {
		// A PIN was just accepted: a list without a record is not Sunshine's.
		return http.StatusBadGateway
	}
	if len(records) == 1 {
		return 0
	}
	remove := surplus(records, g.digestsFor(ctx, records))
	if len(remove) == 0 {
		return 0
	}
	for _, id := range remove {
		payload, err := json.Marshal(unpairRequest{UUID: id})
		if err != nil {
			return http.StatusBadGateway
		}
		b, code := g.sunshineCall(ctx, http.MethodPost, sunshineUnpairURL, payload, cred, maxUnpairReply)
		if code != 0 {
			return code
		}
		if _, ok := pinStatus(b); !ok {
			return http.StatusBadGateway
		}
	}
	after, code := g.listClients(ctx, cred)
	if code != 0 {
		return code
	}
	gone := make(map[string]bool, len(remove))
	for _, id := range remove {
		gone[id] = true
	}
	for _, id := range after {
		if gone[id] {
			return http.StatusBadGateway
		}
	}
	if len(after) == 0 {
		return http.StatusBadGateway
	}
	return 0
}

// pairGate holds new client-certificate connections while a PIN is being
// settled. Sunshine stores the new record before it answers the PIN, and the
// client's next request is its first with the certificate; held back until
// the surplus records are gone, that request never meets a list in which its
// certificate appears twice.
type pairGate struct {
	mu sync.Mutex
	ch chan struct{}
}

// hold closes the gate until the returned function is called. Pairings are
// one at a time (pairSem), so there is one holder.
func (p *pairGate) hold() func() {
	ch := make(chan struct{})
	p.mu.Lock()
	p.ch = ch
	p.mu.Unlock()
	var once sync.Once
	return func() {
		once.Do(func() {
			p.mu.Lock()
			if p.ch == ch {
				p.ch = nil
			}
			p.mu.Unlock()
			close(ch)
		})
	}
}

// wait returns true once the gate is open, false when ctx ends or the wait
// has lasted limit.
func (p *pairGate) wait(ctx context.Context, limit time.Duration) bool {
	var timer *time.Timer
	for {
		p.mu.Lock()
		ch := p.ch
		p.mu.Unlock()
		if ch == nil {
			return true
		}
		if timer == nil {
			timer = time.NewTimer(limit)
			defer timer.Stop()
		}
		select {
		case <-ch:
		case <-ctx.Done():
			return false
		case <-timer.C:
			return false
		}
	}
}
