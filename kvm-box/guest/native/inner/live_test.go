//go:build linux && sunshine_live

package main

// A run against Sunshine itself, not the fixture. It needs the Sunshine of
// the guest image listening on its own loopback (ports 47984, 47989, 47990),
// its admin credential in a file, and automation's TestLiveServePaired
// serving that Sunshine's state file. Nothing here runs in an ordinary test
// run; kvm-box/guest/native/inner/notes.md has the command.
//
// The client below pairs the way Moonlight does: one certificate kept across
// pairings, the PIN entered through this gateway's pairing route, and the
// first certificate request sent the moment the last pairing step answers.

import (
	"bufio"
	"context"
	"crypto"
	"crypto/aes"
	"crypto/rand"
	"crypto/rsa"
	"crypto/sha256"
	"crypto/tls"
	"crypto/x509"
	"crypto/x509/pkix"
	"encoding/hex"
	"encoding/pem"
	"fmt"
	"io"
	"math/big"
	"net"
	"net/http"
	"os"
	"path/filepath"
	"regexp"
	"strings"
	"testing"
	"time"
)

type liveClient struct {
	name string
	key  *rsa.PrivateKey
	der  []byte
	pem  []byte
	sig  []byte
}

func newLiveClient(t *testing.T, name string) *liveClient {
	t.Helper()
	key, err := rsa.GenerateKey(rand.Reader, 2048)
	must(t, err)
	tmpl := &x509.Certificate{SerialNumber: big.NewInt(0), Subject: pkix.Name{CommonName: "NVIDIA GameStream Client"},
		NotBefore: time.Now().Add(-time.Hour), NotAfter: time.Now().Add(20 * 365 * 24 * time.Hour),
		SignatureAlgorithm: x509.SHA256WithRSA}
	der, err := x509.CreateCertificate(rand.Reader, tmpl, tmpl, &key.PublicKey, key)
	must(t, err)
	cert, err := x509.ParseCertificate(der)
	must(t, err)
	return &liveClient{name: name, key: key, der: der, sig: cert.Signature,
		pem: pem.EncodeToMemory(&pem.Block{Type: "CERTIFICATE", Bytes: der})}
}

func ecb(t *testing.T, key, data []byte, encrypt bool) []byte {
	t.Helper()
	block, err := aes.NewCipher(key)
	must(t, err)
	if len(data)%16 != 0 {
		t.Fatalf("%d bytes are not whole blocks", len(data))
	}
	out := make([]byte, len(data))
	for i := 0; i < len(data); i += 16 {
		if encrypt {
			block.Encrypt(out[i:i+16], data[i:i+16])
		} else {
			block.Decrypt(out[i:i+16], data[i:i+16])
		}
	}
	return out
}

func xmlTag(text, tag string) string {
	m := regexp.MustCompile("<" + tag + ">([^<]*)</" + tag + ">").FindStringSubmatch(text)
	if m == nil {
		return ""
	}
	return m[1]
}

func xmlStatus(text string) string {
	m := regexp.MustCompile(`status_code="(\d+)"`).FindStringSubmatch(text)
	if m == nil {
		return ""
	}
	return m[1]
}

func randomHex(t *testing.T, n int) string {
	t.Helper()
	b := make([]byte, n)
	_, err := rand.Read(b)
	must(t, err)
	return hex.EncodeToString(b)
}

type liveRun struct {
	t    *testing.T
	g    *gateway
	addr string
	cred []byte
}

// plain sends one pairing step to Sunshine's HTTP port.
func (l *liveRun) plain(uniqueid, query string) (string, error) {
	c := &http.Client{Timeout: 40 * time.Second, Transport: &http.Transport{DisableKeepAlives: true}}
	resp, err := c.Get("http://127.0.0.1:47989/pair?uniqueid=" + uniqueid + "&uuid=" + randomHex(l.t, 16) +
		"&devicename=roth&updateState=1&" + query)
	if err != nil {
		return "", err
	}
	defer resp.Body.Close()
	b, err := io.ReadAll(io.LimitReader(resp.Body, 1<<20))
	return string(b), err
}

type bufferedConn struct {
	net.Conn
	r *bufio.Reader
}

func (b bufferedConn) Read(p []byte) (int, error) { return b.r.Read(p) }

// secure sends one request with the client's certificate to Sunshine's HTTPS
// port through this gateway's tunnel, as a client outside the guest does.
func (l *liveRun) secure(c *liveClient, path string) (int, string, error) {
	conn, err := net.Dial("tcp", l.addr)
	if err != nil {
		return 0, "", err
	}
	defer conn.Close()
	conn.SetDeadline(time.Now().Add(60 * time.Second))
	if _, err = io.WriteString(conn, tunnelRequest("transport=tcp&port=47984")); err != nil {
		return 0, "", err
	}
	br := bufio.NewReader(conn)
	resp, err := http.ReadResponse(br, &http.Request{Method: http.MethodGet})
	if err != nil {
		return 0, "", err
	}
	if resp.StatusCode != 101 {
		return 0, "", fmt.Errorf("tunnel answered %d", resp.StatusCode)
	}
	tc := tls.Client(bufferedConn{conn, br}, &tls.Config{InsecureSkipVerify: true,
		Certificates: []tls.Certificate{{Certificate: [][]byte{c.der}, PrivateKey: c.key}}})
	if err = tc.Handshake(); err != nil {
		return 0, "", err
	}
	if _, err = io.WriteString(tc, "GET "+path+" HTTP/1.1\r\nHost: 127.0.0.1:47984\r\nConnection: close\r\n\r\n"); err != nil {
		return 0, "", err
	}
	reply, err := http.ReadResponse(bufio.NewReader(tc), &http.Request{Method: http.MethodGet})
	if err != nil {
		return 0, "", err
	}
	defer reply.Body.Close()
	b, _ := io.ReadAll(io.LimitReader(reply.Body, 1<<20))
	return reply.StatusCode, string(b), nil
}

// authorized asks Sunshine for its server information with the client's
// certificate: the request Moonlight repeats for a paired PC.
func (l *liveRun) authorized(c *liveClient) bool {
	l.t.Helper()
	code, body, err := l.secure(c, "/serverinfo?uniqueid="+randomHex(l.t, 8)+"&uuid="+randomHex(l.t, 16))
	if err != nil {
		l.t.Fatalf("client %s: server information: %v", c.name, err)
	}
	ok := code == 200 && xmlStatus(body) == "200" && xmlTag(body, "PairStatus") == "1"
	l.t.Logf("client %s: server information http %d, status %s, PairStatus %s", c.name, code, xmlStatus(body), xmlTag(body, "PairStatus"))
	return ok
}

// pair runs one whole pairing. settle chooses who enters the PIN: this
// gateway's pairing route, or Sunshine's PIN request alone, which is all a
// gateway without clients.go does. It returns the answer to the client's
// first certificate request, sent as soon as the last step answers.
func (l *liveRun) pair(c *liveClient, settle bool) (challengeCode int, challengeStatus string) {
	t := l.t
	t.Helper()
	pinBytes := make([]byte, 2)
	_, err := rand.Read(pinBytes)
	must(t, err)
	pin := fmt.Sprintf("%04d", (int(pinBytes[0])<<8|int(pinBytes[1]))%10000)
	salt := make([]byte, 16)
	_, err = rand.Read(salt)
	must(t, err)
	sum := sha256.Sum256(append(append([]byte{}, salt...), pin...))
	key := sum[:16]
	uniqueid := randomHex(t, 8)

	type result struct {
		code   int
		status string
		err    error
	}
	done := make(chan result, 1)
	go func() {
		fail := func(step string, err error) { done <- result{err: fmt.Errorf("%s: %v", step, err)} }
		a, err := l.plain(uniqueid, "phrase=getservercert&salt="+hex.EncodeToString(salt)+"&clientcert="+hex.EncodeToString(c.pem))
		if err != nil || xmlTag(a, "paired") != "1" {
			fail("getservercert", fmt.Errorf("paired %q, %v", xmlTag(a, "paired"), err))
			return
		}
		challenge := make([]byte, 16)
		rand.Read(challenge)
		b, err := l.plain(uniqueid, "clientchallenge="+hex.EncodeToString(ecb(t, key, challenge, true)))
		if err != nil || xmlTag(b, "paired") != "1" {
			fail("clientchallenge", fmt.Errorf("paired %q, %v", xmlTag(b, "paired"), err))
			return
		}
		raw, err := hex.DecodeString(xmlTag(b, "challengeresponse"))
		if err != nil || len(raw) < 48 || len(raw)%16 != 0 {
			fail("challengeresponse", fmt.Errorf("%d bytes, %v", len(raw), err))
			return
		}
		mine := ecb(t, key, raw, false)
		secret := make([]byte, 16)
		rand.Read(secret)
		h := sha256.New()
		h.Write(mine[32:48])
		h.Write(c.sig)
		h.Write(secret)
		cc, err := l.plain(uniqueid, "serverchallengeresp="+hex.EncodeToString(ecb(t, key, h.Sum(nil), true)))
		if err != nil || xmlTag(cc, "paired") != "1" {
			fail("serverchallengeresp", fmt.Errorf("paired %q, %v", xmlTag(cc, "paired"), err))
			return
		}
		digest := sha256.Sum256(secret)
		sig, err := rsa.SignPKCS1v15(rand.Reader, c.key, crypto.SHA256, digest[:])
		if err != nil {
			fail("sign", err)
			return
		}
		d, err := l.plain(uniqueid, "clientpairingsecret="+hex.EncodeToString(append(secret, sig...)))
		if err != nil || xmlTag(d, "paired") != "1" {
			fail("clientpairingsecret", fmt.Errorf("paired %q, %v", xmlTag(d, "paired"), err))
			return
		}
		// No pause: the record is stored, the PIN request may not have
		// answered yet.
		code, body, err := l.secure(c, "/pair?uniqueid="+uniqueid+"&uuid="+randomHex(t, 16)+
			"&devicename=roth&updateState=1&phrase=pairchallenge")
		if err != nil {
			fail("pairchallenge", err)
			return
		}
		done <- result{code: code, status: xmlStatus(body)}
	}()

	// The PIN, once Sunshine shows the pairing as pending.
	entered := false
	for deadline := time.Now().Add(20 * time.Second); !entered && time.Now().Before(deadline); time.Sleep(100 * time.Millisecond) {
		if settle {
			resp, body := exchange(t, l.addr, pairRequest(`{"pin":"`+pin+`","name":"live `+c.name+`"}`))
			switch {
			case resp.StatusCode == 409:
			case resp.StatusCode == 200 && body == `{"status":true}`:
				entered = true
			default:
				t.Fatalf("client %s: pairing route answered %d %s", c.name, resp.StatusCode, body)
			}
			continue
		}
		ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
		status, code := l.g.pairSunshine(ctx, l.cred, pin, "live "+c.name)
		cancel()
		switch {
		case code == 409:
		case code == 0 && status:
			entered = true
		default:
			t.Fatalf("client %s: Sunshine's PIN request: status %v, code %d", c.name, status, code)
		}
	}
	if !entered {
		t.Fatalf("client %s: no pending pairing appeared", c.name)
	}
	select {
	case r := <-done:
		if r.err != nil {
			t.Fatalf("client %s: %v", c.name, r.err)
		}
		t.Logf("client %s: paired (settled by the gateway: %v); first certificate request http %d, status %s", c.name, settle, r.code, r.status)
		return r.code, r.status
	case <-time.After(60 * time.Second):
		t.Fatalf("client %s: pairing did not finish", c.name)
	}
	return 0, ""
}

func (l *liveRun) records() int {
	l.t.Helper()
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()
	records, code := l.g.listClients(ctx, l.cred)
	if code != 0 {
		l.t.Fatalf("listing the records: %d", code)
	}
	return len(records)
}

func TestLiveSunshinePairAgain(t *testing.T) {
	credFile, sockDir := os.Getenv("KVMBOX_LIVE_CREDENTIAL"), os.Getenv("KVMBOX_LIVE_SOCKET_DIR")
	if credFile == "" || sockDir == "" {
		t.Skip("not a live run")
	}
	raw, err := os.ReadFile(credFile)
	must(t, err)
	cred := strings.TrimSpace(string(raw))
	cfg := testConfig(t)
	prod := productionConfig()
	cfg.sunshineAddr, cfg.tunnelTarget, cfg.digestRetry = prod.sunshineAddr, prod.tunnelTarget, prod.digestRetry
	writeCredential(t, &cfg, cred)
	cfg.automationSocket = filepath.Join(sockDir, "desktop.sock")
	cfg.automationUID, cfg.automationGID, cfg.automationPeerUID = uint32(os.Getuid()), uint32(os.Getgid()), uint32(os.Getuid())
	for deadline := time.Now().Add(60 * time.Second); ; time.Sleep(200 * time.Millisecond) {
		ready := true
		for _, target := range []string{"127.0.0.1:47984", "127.0.0.1:47989", "127.0.0.1:47990", cfg.automationSocket} {
			network := "tcp"
			if strings.HasPrefix(target, "/") {
				network = "unix"
			}
			c, err := net.DialTimeout(network, target, time.Second)
			if err != nil {
				ready = false
				break
			}
			c.Close()
		}
		if ready {
			break
		}
		if time.Now().After(deadline) {
			t.Fatal("Sunshine or the paired-records service is not listening")
		}
	}
	g := newGateway(cfg)
	defer g.Close()
	ln, err := net.Listen("tcp", "127.0.0.1:0")
	must(t, err)
	go g.Serve(ln)
	l := &liveRun{t: t, g: g, addr: ln.Addr().String(), cred: []byte(cred)}
	a, b := newLiveClient(t, "A"), newLiveClient(t, "B")

	if n := l.records(); n != 0 {
		t.Fatalf("%d records before the first pairing", n)
	}
	// A pairs, and pairs again and again: one record, authorized each time,
	// from its first certificate request on.
	for round := 1; round <= 3; round++ {
		if code, status := l.pair(a, true); code != 200 || status != "200" {
			t.Fatalf("round %d: first certificate request http %d, status %s", round, code, status)
		}
		if !l.authorized(a) || l.records() != 1 {
			t.Fatalf("round %d: A not authorized or %d records", round, l.records())
		}
	}
	// A second device beside it; neither loses its pairing to the other.
	if code, status := l.pair(b, true); code != 200 || status != "200" {
		t.Fatalf("B: first certificate request http %d, status %s", code, status)
	}
	if !l.authorized(a) || !l.authorized(b) || l.records() != 2 {
		t.Fatalf("after B paired: %d records", l.records())
	}
	if code, status := l.pair(a, true); code != 200 || status != "200" {
		t.Fatalf("A beside B: first certificate request http %d, status %s", code, status)
	}
	if !l.authorized(a) || !l.authorized(b) || l.records() != 2 {
		t.Fatalf("after A paired beside B: %d records", l.records())
	}

	// The defect itself, so that the checks above mean something: B pairs
	// again with the PIN entered at Sunshine alone. Sunshine then holds two
	// records of B's certificate and refuses it.
	l.pair(b, false)
	if n := l.records(); n != 3 {
		t.Fatalf("%d records after a pairing Sunshine was left to keep", n)
	}
	if l.authorized(b) {
		t.Fatal("this Sunshine accepts a certificate with two records: the cleanup is not what makes pairing again work")
	}
	if !l.authorized(a) {
		t.Fatal("A lost its pairing to B's second record")
	}
	// And the way out of that state: pairing once more through the gateway.
	if code, status := l.pair(b, true); code != 200 || status != "200" {
		t.Fatalf("B after the defect: first certificate request http %d, status %s", code, status)
	}
	if !l.authorized(a) || !l.authorized(b) || l.records() != 2 {
		t.Fatalf("after healing: %d records", l.records())
	}

	// Without the desktop's digests only the newest record stays.
	g.cfg.automationSocket = filepath.Join(sockDir, "absent.sock")
	if code, status := l.pair(a, true); code != 200 || status != "200" {
		t.Fatalf("A without digests: first certificate request http %d, status %s", code, status)
	}
	if !l.authorized(a) || l.authorized(b) || l.records() != 1 {
		t.Fatalf("without digests: %d records", l.records())
	}
}
