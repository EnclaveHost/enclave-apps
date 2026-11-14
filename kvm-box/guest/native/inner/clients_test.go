//go:build linux

package main

import (
	"bufio"
	"context"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"io"
	"net"
	"net/http"
	"strings"
	"sync/atomic"
	"testing"
	"time"
)

// The fixture's side of clients.go: Sunshine's paired records.

type fixtureRecord struct {
	uuid, cert string
}

func fixtureDigest(cert string) string {
	sum := sha256.Sum256([]byte(cert))
	return hex.EncodeToString(sum[:])
}

// addRecord stores a record the way Sunshine does: at the end, whatever is
// already there.
func (s *sunshineFixture) addRecord() {
	s.mu.Lock()
	defer s.mu.Unlock()
	cert := s.pairAs
	if cert == "" {
		cert = "a"
	}
	s.serial++
	s.records = append(s.records, fixtureRecord{fmt.Sprintf("00000000-0000-4000-8000-%012d", s.serial), cert})
}

// certs returns the certificates of the records, oldest first.
func (s *sunshineFixture) certs() string {
	s.mu.Lock()
	defer s.mu.Unlock()
	out := make([]string, len(s.records))
	for i, r := range s.records {
		out[i] = r.cert
	}
	return strings.Join(out, ",")
}

func (s *sunshineFixture) removed() int {
	s.mu.Lock()
	defer s.mu.Unlock()
	return len(s.unpaired)
}

func (s *sunshineFixture) serveClients(w http.ResponseWriter, r *http.Request) {
	// Sunshine's request-forgery check: a client that is not a browser names
	// no origin.
	if r.Header.Get("Origin") != "" || r.Header.Get("Referer") != "" {
		w.WriteHeader(http.StatusForbidden)
		return
	}
	if r.URL.Path == "/api/clients/list" {
		if r.Method != http.MethodGet {
			w.WriteHeader(http.StatusMethodNotAllowed)
			return
		}
		s.lists.Add(1)
		if s.listGate != nil {
			<-s.listGate
		}
		s.mu.Lock()
		defer s.mu.Unlock()
		if s.clientsBody != "" {
			io.WriteString(w, s.clientsBody)
			return
		}
		type named struct {
			Name    string `json:"name"`
			UUID    string `json:"uuid"`
			Enabled bool   `json:"enabled"`
		}
		list := []named{}
		for _, rec := range s.records {
			list = append(list, named{"KVM Box owner", rec.uuid, true})
		}
		b, _ := json.Marshal(map[string]any{"named_certs": list, "status": true})
		w.Write(b)
		return
	}
	var req struct {
		UUID string `json:"uuid"`
	}
	b, _ := io.ReadAll(r.Body)
	if r.Method != http.MethodPost || !strings.HasPrefix(r.Header.Get("Content-Type"), "application/json") ||
		json.Unmarshal(b, &req) != nil {
		w.WriteHeader(http.StatusBadRequest)
		return
	}
	s.mu.Lock()
	s.unpaired = append(s.unpaired, req.UUID)
	removed := false
	if !s.keepOnUnpair {
		kept := s.records[:0]
		for _, rec := range s.records {
			if rec.uuid == req.UUID {
				removed = true
				continue
			}
			kept = append(kept, rec)
		}
		s.records = kept
	}
	s.mu.Unlock()
	if s.unpairBody != "" {
		io.WriteString(w, s.unpairBody)
		return
	}
	fmt.Fprintf(w, `{"status":%t}`, removed)
}

// digestBody is the desktop's answer for the fixture's records; drop leaves
// out that many of the newest, as a state file read before a write would.
func (s *sunshineFixture) digestBody(drop int) string {
	s.mu.Lock()
	defer s.mu.Unlock()
	type client struct {
		UUID string `json:"uuid"`
		Cert string `json:"cert_sha256"`
	}
	list := []client{}
	for i, rec := range s.records {
		if i >= len(s.records)-drop {
			break
		}
		list = append(list, client{rec.uuid, fixtureDigest(rec.cert)})
	}
	b, _ := json.Marshal(map[string]any{"ok": true, "clients": list})
	return string(b)
}

// startDigests runs the desktop automation service's part: the digest of
// each record's certificate. answer may replace the n-th reply.
func startDigests(t *testing.T, cfg *config, s *sunshineFixture, answer func(n int) string) *atomic.Int32 {
	t.Helper()
	asked := new(atomic.Int32)
	startAutomation(t, cfg, func(c net.Conn) {
		r, e := http.ReadRequest(bufio.NewReader(c))
		if e != nil {
			return
		}
		b, _ := io.ReadAll(r.Body)
		r.Body.Close()
		if r.Method != http.MethodPost || r.URL.Path != "/paired" || string(b) != "{}" {
			io.WriteString(c, "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
			return
		}
		n := int(asked.Add(1))
		body := ""
		if answer != nil {
			body = answer(n)
		}
		if body == "" {
			body = s.digestBody(0)
		}
		io.WriteString(c, automationReply(body, ""))
	})
	return asked
}

const onePending = `{"pairings":[{"id":"abc-1"}]}`

func pairAs(t *testing.T, addr string, s *sunshineFixture, cert string, code int) string {
	t.Helper()
	s.mu.Lock()
	s.pairAs = cert
	s.mu.Unlock()
	resp, body := exchange(t, addr, pairRequest(`{"pin":"1234"}`))
	expectStatus(t, resp, code)
	select {
	case <-s.posted:
	default:
	}
	return body
}

func TestSurplusKeepsTheNewestRecordOfEachCertificate(t *testing.T) {
	ids := func(n int) []string {
		out := make([]string, n)
		for i := range out {
			out[i] = fmt.Sprint("id", i)
		}
		return out
	}
	digests := func(certs string) map[string]string {
		out := map[string]string{}
		for i, c := range strings.Split(certs, "") {
			out[fmt.Sprint("id", i)] = fixtureDigest(c)
		}
		return out
	}
	for _, tc := range []struct{ certs, want string }{
		{"a", ""},
		{"ab", ""},
		{"aa", "id0"},
		{"aba", "id0"},
		{"aaaaabb", "id0 id1 id2 id3 id5"},
		{"abcabc", "id0 id1 id2"},
		{"abba", "id0 id1"},
	} {
		if got := strings.Join(surplus(ids(len(tc.certs)), digests(tc.certs)), " "); got != tc.want {
			t.Errorf("%s: removes %q, want %q", tc.certs, got, tc.want)
		}
	}
	if got := surplus(nil, nil); len(got) != 0 {
		t.Errorf("empty list: %v", got)
	}
	// Without a digest for every record only the newest record is kept.
	partial := digests("ab")
	for name, d := range map[string]map[string]string{"none": nil, "empty": {}, "partial": partial} {
		if got := strings.Join(surplus(ids(3), d), " "); got != "id0 id1" {
			t.Errorf("%s digests: removes %q", name, got)
		}
		if got := surplus(ids(1), nil); len(got) != 0 {
			t.Errorf("%s digests: single record removed", name)
		}
	}
	// Whatever the digests say, a record always remains, and it is the newest.
	for n := 1; n <= 6; n++ {
		for mask := 0; mask < 1<<n; mask++ {
			certs := ""
			for i := 0; i < n; i++ {
				certs += string(rune('a' + (mask>>i)&1))
			}
			got := surplus(ids(n), digests(certs))
			if len(got) >= n {
				t.Fatalf("%s: every record removed", certs)
			}
			for _, id := range got {
				if id == fmt.Sprint("id", n-1) {
					t.Fatalf("%s: newest record removed", certs)
				}
			}
		}
	}
}

func TestClientListAndDigestParsing(t *testing.T) {
	got, ok := clientUUIDs([]byte(`{"named_certs":[{"name":"x","uuid":"AB-1","enabled":true},{"uuid":"cd-2"}],"status":true}`))
	if !ok || strings.Join(got, " ") != "AB-1 cd-2" {
		t.Fatalf("list %v %v", got, ok)
	}
	if got, ok := clientUUIDs([]byte(`{"named_certs":[]}`)); !ok || len(got) != 0 {
		t.Fatal("empty list refused")
	}
	many := make([]string, maxClients+1)
	for i := range many {
		many[i] = fmt.Sprintf(`{"uuid":"%d"}`, i)
	}
	for name, raw := range map[string]string{
		"html":          `<html>`,
		"no list":       `{"status":true}`,
		"null list":     `{"named_certs":null}`,
		"object list":   `{"named_certs":{}}`,
		"missing uuid":  `{"named_certs":[{"name":"x"}]}`,
		"numeric uuid":  `{"named_certs":[{"uuid":7}]}`,
		"empty uuid":    `{"named_certs":[{"uuid":""}]}`,
		"uuid path":     `{"named_certs":[{"uuid":"../x"}]}`,
		"uuid quote":    `{"named_certs":[{"uuid":"a\"b"}]}`,
		"long uuid":     `{"named_certs":[{"uuid":"` + strings.Repeat("a", 65) + `"}]}`,
		"repeated uuid": `{"named_certs":[{"uuid":"a"},{"uuid":"a"}]}`,
		"too many":      `{"named_certs":[` + strings.Join(many, ",") + `]}`,
		"trailing":      `{"named_certs":[]} x`,
	} {
		if got, ok := clientUUIDs([]byte(raw)); ok {
			t.Errorf("%s accepted: %v", name, got)
		}
	}
	d := fixtureDigest("a")
	m, ok := recordDigests([]byte(`{"ok":true,"clients":[{"uuid":"a-1","cert_sha256":"` + d + `"}]}`))
	if !ok || len(m) != 1 || m["a-1"] != d {
		t.Fatalf("digests %v %v", m, ok)
	}
	for name, raw := range map[string]string{
		"not ok":         `{"ok":false,"clients":[]}`,
		"no clients":     `{"ok":true}`,
		"upper digest":   `{"ok":true,"clients":[{"uuid":"a","cert_sha256":"` + strings.ToUpper(d) + `"}]}`,
		"short digest":   `{"ok":true,"clients":[{"uuid":"a","cert_sha256":"` + d[:63] + `"}]}`,
		"missing digest": `{"ok":true,"clients":[{"uuid":"a"}]}`,
		"bad uuid":       `{"ok":true,"clients":[{"uuid":"a b","cert_sha256":"` + d + `"}]}`,
		"repeated uuid":  `{"ok":true,"clients":[{"uuid":"a","cert_sha256":"` + d + `"},{"uuid":"a","cert_sha256":"` + d + `"}]}`,
	} {
		if m, ok := recordDigests([]byte(raw)); ok {
			t.Errorf("%s accepted: %v", name, m)
		}
	}
}

func TestPairAgainLeavesOneRecordPerCertificate(t *testing.T) {
	cfg := testConfig(t)
	writeCredential(t, &cfg, testCredential)
	s := &sunshineFixture{list: onePending, reply: `{"status":true}`}
	startSunshine(t, &cfg, s)
	asked := startDigests(t, &cfg, s, nil)
	addr := startGateway(t, cfg)
	for _, step := range []struct{ cert, want string }{
		{"a", "a"},       // first device
		{"b", "a,b"},     // a second device stays beside it
		{"a", "b,a"},     // the first pairs again: its older record goes
		{"a", "b,a"},     // and again
		{"b", "a,b"},     // the second pairs again
		{"c", "a,b,c"},   // a third device
		{"b", "a,c,b"},   // only the repeated certificate loses a record
		{"c", "a,b,c"},   //
		{"a", "b,c,a"},   //
		{"d", "b,c,a,d"}, //
	} {
		if body := pairAs(t, addr, s, step.cert, 200); body != `{"status":true}` {
			t.Fatalf("pairing %s: body %q", step.cert, body)
		}
		if got := s.certs(); got != step.want {
			t.Fatalf("after pairing %s: records %s, want %s", step.cert, got, step.want)
		}
	}
	if asked.Load() == 0 || s.other.Load() != 0 {
		t.Fatalf("digests asked %d times, unknown Sunshine paths %d", asked.Load(), s.other.Load())
	}
}

func TestPairHealsRecordsLeftByEarlierPairings(t *testing.T) {
	cfg := testConfig(t)
	writeCredential(t, &cfg, testCredential)
	s := &sunshineFixture{list: onePending, reply: `{"status":true}`}
	// What a guest without this cleanup accumulates: one client paired five
	// times, another twice. Neither can authenticate.
	for _, cert := range []string{"a", "a", "a", "a", "a", "b", "b"} {
		s.pairAs = cert
		s.addRecord()
	}
	startSunshine(t, &cfg, s)
	startDigests(t, &cfg, s, nil)
	addr := startGateway(t, cfg)
	pairAs(t, addr, s, "a", 200)
	if got := s.certs(); got != "b,a" {
		t.Fatalf("records %s", got)
	}
	if n := s.removed(); n != 6 {
		t.Fatalf("%d removals", n)
	}
}

func TestPairWithoutDigestsKeepsTheNewestRecord(t *testing.T) {
	unavailable := automationReply(`{"ok":false,"error":"paired clients unavailable"}`, "")
	unavailable = strings.Replace(unavailable, "200 OK", "503 Service Unavailable", 1)
	for name, setup := range map[string]func(t *testing.T, cfg *config, s *sunshineFixture) *atomic.Int32{
		"no desktop service": func(*testing.T, *config, *sunshineFixture) *atomic.Int32 { return nil },
		"state unreadable": func(t *testing.T, cfg *config, s *sunshineFixture) *atomic.Int32 {
			asked := new(atomic.Int32)
			startAutomation(t, cfg, func(c net.Conn) {
				asked.Add(1)
				fixedAutomationReply(unavailable)(c)
			})
			return asked
		},
		"state always behind": func(t *testing.T, cfg *config, s *sunshineFixture) *atomic.Int32 {
			return startDigests(t, cfg, s, func(int) string { return s.digestBody(1) })
		},
		"answer malformed": func(t *testing.T, cfg *config, s *sunshineFixture) *atomic.Int32 {
			return startDigests(t, cfg, s, func(int) string { return `{"ok":true,"clients":[{"uuid":"x"}]}` })
		},
	} {
		t.Run(name, func(t *testing.T) {
			cfg := testConfig(t)
			writeCredential(t, &cfg, testCredential)
			s := &sunshineFixture{list: onePending, reply: `{"status":true}`}
			startSunshine(t, &cfg, s)
			asked := setup(t, &cfg, s)
			addr := startGateway(t, cfg)
			for _, step := range []struct{ cert, want string }{{"a", "a"}, {"b", "b"}, {"b", "b"}, {"a", "a"}} {
				pairAs(t, addr, s, step.cert, 200)
				if got := s.certs(); got != step.want {
					t.Fatalf("after pairing %s: records %s, want %s", step.cert, got, step.want)
				}
			}
			// Three tries each time there was something to decide.
			if asked != nil && asked.Load() != 3*digestTries {
				t.Fatalf("digests asked %d times", asked.Load())
			}
		})
	}
}

func TestPairUsesTheSecondDigestAnswer(t *testing.T) {
	cfg := testConfig(t)
	writeCredential(t, &cfg, testCredential)
	s := &sunshineFixture{list: onePending, reply: `{"status":true}`}
	for _, cert := range []string{"a", "b"} {
		s.pairAs = cert
		s.addRecord()
	}
	startSunshine(t, &cfg, s)
	// The first read misses the record just stored, as a state file read
	// before Sunshine's write would; the second has it.
	asked := startDigests(t, &cfg, s, func(n int) string {
		if n == 1 {
			return s.digestBody(1)
		}
		return ""
	})
	addr := startGateway(t, cfg)
	pairAs(t, addr, s, "a", 200)
	// The other device kept its record: the fallback did not run.
	if got := s.certs(); got != "b,a" || asked.Load() != 2 {
		t.Fatalf("records %s after %d answers", got, asked.Load())
	}
}

func TestPairReportsAnUnsettledList(t *testing.T) {
	earlier := func(s *sunshineFixture) {
		s.pairAs = "a"
		s.addRecord()
	}
	for name, tc := range map[string]struct {
		prepare func(s *sunshineFixture)
		code    int
	}{
		"PIN accepted, no record": {func(s *sunshineFixture) { s.noRecord = true }, 502},
		"list is html":            {func(s *sunshineFixture) { s.clientsBody = `<html>clients</html>` }, 502},
		"list without records":    {func(s *sunshineFixture) { s.clientsBody = `{"status":true}` }, 502},
		"list oversized": {func(s *sunshineFixture) {
			s.clientsBody = `{"named_certs":[],"pad":"` + strings.Repeat("p", maxClientList) + `"}`
		}, 502},
		"record with a bad name":  {func(s *sunshineFixture) { s.clientsBody = `{"named_certs":[{"uuid":"../../x"}]}` }, 502},
		"removal not carried out": {func(s *sunshineFixture) { earlier(s); s.keepOnUnpair = true }, 502},
		"removal answers html":    {func(s *sunshineFixture) { earlier(s); s.unpairBody = `<html>no</html>` }, 502},
	} {
		t.Run(name, func(t *testing.T) {
			cfg := testConfig(t)
			writeCredential(t, &cfg, testCredential)
			s := &sunshineFixture{list: onePending, reply: `{"status":true}`}
			tc.prepare(s)
			startSunshine(t, &cfg, s)
			startDigests(t, &cfg, s, nil)
			addr := startGateway(t, cfg)
			if body := pairAs(t, addr, s, "a", tc.code); body != `{"error":"bad gateway"}` {
				t.Fatalf("body %q", body)
			}
		})
	}
	t.Run("a refused PIN changes nothing", func(t *testing.T) {
		cfg := testConfig(t)
		writeCredential(t, &cfg, testCredential)
		s := &sunshineFixture{list: onePending, reply: `{"status":false}`}
		earlier(s)
		earlier(s)
		startSunshine(t, &cfg, s)
		asked := startDigests(t, &cfg, s, nil)
		addr := startGateway(t, cfg)
		if body := pairAs(t, addr, s, "a", 200); body != `{"status":false}` {
			t.Fatalf("body %q", body)
		}
		if s.lists.Load() != 0 || asked.Load() != 0 || s.removed() != 0 || s.certs() != "a,a" {
			t.Fatal("records touched without an accepted PIN")
		}
	})
}

func TestPairedIsNotAWebRoute(t *testing.T) {
	cfg := testConfig(t)
	s := &sunshineFixture{}
	asked := startDigests(t, &cfg, s, nil)
	addr := startGateway(t, cfg)
	for _, raw := range []string{
		post("/paired", "application/json", `{}`),
		post("/paired", "application/json", `{}`, "X-KVM-Box-Request: 1\r\n"),
		"GET /paired HTTP/1.1\r\n" + hostLine + "\r\n",
		post("/api/moonlight/paired", "application/json", `{}`),
	} {
		resp, _ := exchange(t, addr, raw)
		expectStatus(t, resp, 404)
	}
	if asked.Load() != 0 {
		t.Fatal("the desktop was asked on behalf of a web request")
	}
	if classify("/paired") != classify("/no-such-route") {
		t.Fatal("/paired has a route")
	}
}

func TestPairGate(t *testing.T) {
	var p pairGate
	if !p.wait(context.Background(), time.Millisecond) {
		t.Fatal("open gate held a caller")
	}
	open := p.hold()
	start := time.Now()
	if p.wait(context.Background(), 30*time.Millisecond) || time.Since(start) < 30*time.Millisecond {
		t.Fatal("closed gate let a caller through")
	}
	ctx, cancel := context.WithCancel(context.Background())
	cancel()
	if p.wait(ctx, time.Minute) {
		t.Fatal("cancelled caller let through")
	}
	passed := make(chan bool, 3)
	for i := 0; i < 3; i++ {
		go func() { passed <- p.wait(context.Background(), 5*time.Second) }()
	}
	select {
	case <-passed:
		t.Fatal("caller passed a closed gate")
	case <-time.After(50 * time.Millisecond):
	}
	open()
	open() // releasing twice is harmless
	for i := 0; i < 3; i++ {
		select {
		case ok := <-passed:
			if !ok {
				t.Fatal("caller refused after the gate opened")
			}
		case <-time.After(2 * time.Second):
			t.Fatal("caller still held after the gate opened")
		}
	}
	// A second pairing holds callers that arrive during it.
	open2 := p.hold()
	go func() { passed <- p.wait(context.Background(), 5*time.Second) }()
	select {
	case <-passed:
		t.Fatal("caller passed the second hold")
	case <-time.After(30 * time.Millisecond):
	}
	open2()
	if !<-passed {
		t.Fatal("caller refused after the second hold")
	}
}

// While a PIN is being settled a connection to Sunshine's certificate port
// waits; when it goes through, the certificate has one record.
func TestCertificateConnectionsWaitForTheSettledList(t *testing.T) {
	cfg := testConfig(t)
	writeCredential(t, &cfg, testCredential)
	s := &sunshineFixture{list: onePending, reply: `{"status":true}`, pinGate: make(chan struct{})}
	s.pairAs = "a"
	s.addRecord()
	startSunshine(t, &cfg, s)
	startDigests(t, &cfg, s, nil)

	// Stand-ins for Sunshine's two request ports. Each tells what the paired
	// records were when a connection arrived.
	listen := func() (net.Listener, chan string) {
		ln, err := net.Listen("tcp", "127.0.0.1:0")
		must(t, err)
		t.Cleanup(func() { ln.Close() })
		seen := make(chan string, 4)
		go func() {
			for {
				c, err := ln.Accept()
				if err != nil {
					return
				}
				seen <- s.certs()
				c.Close()
			}
		}()
		return ln, seen
	}
	https, httpsSeen := listen()
	plain, plainSeen := listen()
	refused := closedAddr(t)
	cfg.tunnelTarget = func(transport, port string) string {
		switch {
		case transport == "tcp" && port == "47984":
			return https.Addr().String()
		case transport == "tcp" && port == "47989":
			return plain.Addr().String()
		}
		return refused
	}
	addr := startGateway(t, cfg)

	dial := func(port string) chan int {
		done := make(chan int, 1)
		go func() {
			c, err := net.Dial("tcp", addr)
			if err != nil {
				done <- 0
				return
			}
			defer c.Close()
			c.SetDeadline(time.Now().Add(10 * time.Second))
			io.WriteString(c, tunnelRequest("transport=tcp&port="+port))
			resp, err := http.ReadResponse(bufio.NewReader(c), &http.Request{Method: http.MethodGet})
			if err != nil {
				done <- 0
				return
			}
			done <- resp.StatusCode
		}()
		return done
	}
	// Before any pairing the certificate port is not held.
	if code := <-dial("47984"); code != 101 {
		t.Fatalf("idle gateway: status %d", code)
	}
	<-httpsSeen

	paired := make(chan string, 1)
	go func() {
		c, err := net.Dial("tcp", addr)
		if err != nil {
			paired <- err.Error()
			return
		}
		defer c.Close()
		c.SetDeadline(time.Now().Add(10 * time.Second))
		io.WriteString(c, pairRequest(`{"pin":"1234"}`))
		resp, err := http.ReadResponse(bufio.NewReader(c), nil)
		if err != nil {
			paired <- err.Error()
			return
		}
		b, _ := io.ReadAll(resp.Body)
		paired <- string(b)
	}()
	select {
	case <-s.posted: // Sunshine has the PIN and has not answered
	case <-time.After(5 * time.Second):
		t.Fatal("PIN never reached Sunshine")
	}
	held := dial("47984")
	// The pairing exchange itself runs on the other port and is not held.
	if code := <-dial("47989"); code != 101 {
		t.Fatalf("pairing port: status %d", code)
	}
	<-plainSeen
	select {
	case code := <-held:
		t.Fatalf("certificate connection answered %d while the PIN was being settled", code)
	case got := <-httpsSeen:
		t.Fatalf("certificate connection reached Sunshine with records %s", got)
	case <-time.After(150 * time.Millisecond):
	}
	close(s.pinGate)
	select {
	case got := <-httpsSeen:
		if got != "a" {
			t.Fatalf("certificate connection met records %s", got)
		}
	case <-time.After(5 * time.Second):
		t.Fatal("certificate connection never released")
	}
	if code := <-held; code != 101 {
		t.Fatalf("held connection: status %d", code)
	}
	if body := <-paired; body != `{"status":true}` {
		t.Fatalf("pairing answered %q", body)
	}
}

func TestHeldCertificateConnectionGivesUp(t *testing.T) {
	cfg := testConfig(t)
	cfg.pairTimeout = 80 * time.Millisecond
	cfg.tidyTimeout = 40 * time.Millisecond
	g := newGateway(cfg)
	defer g.Close()
	ln, err := net.Listen("tcp", "127.0.0.1:0")
	must(t, err)
	go g.Serve(ln)
	open := g.pairGate.hold()
	defer open()
	start := time.Now()
	resp, body := exchange(t, ln.Addr().String(), tunnelRequest("transport=tcp&port=47984"))
	expectStatus(t, resp, 503)
	if !strings.Contains(body, "service unavailable") || time.Since(start) < 120*time.Millisecond {
		t.Fatalf("body %q after %v", body, time.Since(start))
	}
}

// The page going away after Sunshine accepted the PIN does not leave the
// certificate with two records.
func TestSettlingOutlivesThePageRequest(t *testing.T) {
	cfg := testConfig(t)
	writeCredential(t, &cfg, testCredential)
	s := &sunshineFixture{list: onePending, reply: `{"status":true}`, listGate: make(chan struct{})}
	s.pairAs = "a"
	s.addRecord()
	startSunshine(t, &cfg, s)
	startDigests(t, &cfg, s, nil)
	addr := startGateway(t, cfg)
	c, err := net.Dial("tcp", addr)
	must(t, err)
	_, err = io.WriteString(c, pairRequest(`{"pin":"1234"}`))
	must(t, err)
	deadline := time.Now().Add(5 * time.Second)
	for s.lists.Load() == 0 {
		if time.Now().After(deadline) {
			t.Fatal("the records were never listed")
		}
		time.Sleep(5 * time.Millisecond)
	}
	c.Close()
	time.Sleep(50 * time.Millisecond) // let the gateway notice
	close(s.listGate)
	for s.certs() != "a" {
		if time.Now().After(deadline) {
			t.Fatalf("records %s after the page left", s.certs())
		}
		time.Sleep(5 * time.Millisecond)
	}
}
