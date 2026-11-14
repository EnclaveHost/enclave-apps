//go:build linux

package main

import (
	"crypto/ecdsa"
	"crypto/elliptic"
	"crypto/rand"
	"crypto/sha256"
	"crypto/x509"
	"crypto/x509/pkix"
	"encoding/hex"
	"encoding/json"
	"encoding/pem"
	"fmt"
	"math/big"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"
)

// testCert returns a client certificate's PEM text and the digest of its DER.
func testCert(t *testing.T, name string) (string, string) {
	t.Helper()
	key, err := ecdsa.GenerateKey(elliptic.P256(), rand.Reader)
	if err != nil {
		t.Fatal(err)
	}
	tmpl := &x509.Certificate{SerialNumber: big.NewInt(1), Subject: pkix.Name{CommonName: name},
		NotBefore: time.Unix(0, 0), NotAfter: time.Unix(1<<31, 0)}
	der, err := x509.CreateCertificate(rand.Reader, tmpl, tmpl, &key.PublicKey, key)
	if err != nil {
		t.Fatal(err)
	}
	sum := sha256.Sum256(der)
	return string(pem.EncodeToMemory(&pem.Block{Type: "CERTIFICATE", Bytes: der})), hex.EncodeToString(sum[:])
}

// stateFile is shaped as Sunshine's property-tree encoder writes it: every
// value a string, the admin credential beside the records.
func stateFile(t *testing.T, devices string) []byte {
	t.Helper()
	return []byte(`{"username":"owner","salt":"0123456789abcdef","password":"` + strings.Repeat("ab", 32) +
		`","root":{"uniqueid":"11111111-2222-3333-4444-555555555555","named_devices":` + devices + `}}`)
}

func device(uuid, cert string) string {
	b, _ := json.Marshal(map[string]string{"name": "KVM Box owner", "cert": cert, "uuid": uuid, "enabled": "true"})
	return string(b)
}

func TestPairedRecordsByCertificateDigest(t *testing.T) {
	a, da := testCert(t, "a")
	b, db := testCert(t, "b")
	// The same certificate spelled with CRLF line ends and trailing space.
	spelled := strings.ReplaceAll(a, "\n", "\r\n") + "  \n"
	got, err := parsePaired(stateFile(t, "["+device("AAAAAAAA-0000-0000-0000-000000000001", a)+","+
		device("AAAAAAAA-0000-0000-0000-000000000002", b)+","+device("AAAAAAAA-0000-0000-0000-000000000003", spelled)+"]"))
	if err != nil {
		t.Fatal(err)
	}
	want := []pairedClient{{"AAAAAAAA-0000-0000-0000-000000000001", da}, {"AAAAAAAA-0000-0000-0000-000000000002", db},
		{"AAAAAAAA-0000-0000-0000-000000000003", da}}
	if fmt.Sprint(got) != fmt.Sprint(want) {
		t.Fatalf("records %v", got)
	}
	// Text that is not a certificate still compares equal to itself only.
	x, y := certDigest("not a certificate"), certDigest(" not a certificate\n")
	if x != y || x == certDigest("another") || x == da {
		t.Fatal("text digest")
	}
}

func TestPairedEmptyAndRefusedStates(t *testing.T) {
	cert, _ := testCert(t, "a")
	for _, devices := range []string{`""`, `[]`, `null`} {
		got, err := parsePaired(stateFile(t, devices))
		if err != nil || len(got) != 0 {
			t.Fatalf("%s: %v %v", devices, got, err)
		}
	}
	if got, err := parsePaired([]byte(`{"username":"owner"}`)); err != nil || len(got) != 0 {
		t.Fatalf("no root: %v %v", got, err)
	}
	many := make([]string, maxPairedClients+1)
	for i := range many {
		many[i] = device(fmt.Sprintf("AAAAAAAA-0000-0000-0000-%012d", i), cert)
	}
	for name, raw := range map[string][]byte{
		"not json":       []byte(`<html>`),
		"devices object": stateFile(t, `{"0":`+device("AAAAAAAA-0000-0000-0000-000000000001", cert)+`}`),
		"missing uuid":   stateFile(t, `[{"name":"x","cert":"c"}]`),
		"missing cert":   stateFile(t, `[{"name":"x","uuid":"AAAAAAAA-0000-0000-0000-000000000001"}]`),
		"empty cert":     stateFile(t, `[`+device("AAAAAAAA-0000-0000-0000-000000000001", " ")+`]`),
		"uuid with path": stateFile(t, `[`+device("../../etc", cert)+`]`),
		"repeated uuid": stateFile(t, `[`+device("AAAAAAAA-0000-0000-0000-000000000001", cert)+`,`+
			device("AAAAAAAA-0000-0000-0000-000000000001", cert)+`]`),
		"too many": stateFile(t, `[`+strings.Join(many, ",")+`]`),
	} {
		if got, err := parsePaired(raw); err == nil {
			t.Fatalf("%s accepted: %v", name, got)
		}
	}
}

func TestPairedRouteReturnsDigestsOnly(t *testing.T) {
	s := testService(t)
	cert, digest := testCert(t, "a")
	s.statePath = filepath.Join(s.dir, "sunshine_state.json")
	if err := os.WriteFile(s.statePath, stateFile(t, "["+device("AAAAAAAA-0000-0000-0000-000000000001", cert)+"]"), 0600); err != nil {
		t.Fatal(err)
	}
	w := httptest.NewRecorder()
	s.ServeHTTP(w, httptest.NewRequest("POST", "/paired", strings.NewReader(`{}`)))
	want := `{"clients":[{"uuid":"AAAAAAAA-0000-0000-0000-000000000001","cert_sha256":"` + digest + `"}],"ok":true}`
	if w.Code != 200 || w.Body.String() != want {
		t.Fatalf("%d %s", w.Code, w.Body.String())
	}
	// Nothing else of the file: not the certificate, not the credential hash.
	for _, secret := range []string{"BEGIN CERTIFICATE", strings.Repeat("ab", 32), "0123456789abcdef", "salt", "password"} {
		if strings.Contains(w.Body.String(), secret) {
			t.Fatal("reply carries", secret)
		}
	}
	for _, body := range []string{``, `{"path":"/etc/passwd"}`, `[]`, `{} {}`} {
		w = httptest.NewRecorder()
		s.ServeHTTP(w, httptest.NewRequest("POST", "/paired", strings.NewReader(body)))
		if w.Code != 400 {
			t.Fatalf("body %q: %d", body, w.Code)
		}
	}
	for _, path := range []string{"/paired?path=/etc/passwd", "/paired/"} {
		w = httptest.NewRecorder()
		s.ServeHTTP(w, httptest.NewRequest("POST", path, strings.NewReader(`{}`)))
		if w.Code != 404 {
			t.Fatal("accepted alternate route", path)
		}
	}
	w = httptest.NewRecorder()
	s.ServeHTTP(w, httptest.NewRequest("GET", "/paired", nil))
	if w.Code != 404 {
		t.Fatal("accepted GET")
	}
	// Commands in flight do not keep the pairing waiting, and the reverse.
	s.slots <- struct{}{}
	s.slots <- struct{}{}
	w = httptest.NewRecorder()
	s.ServeHTTP(w, httptest.NewRequest("POST", "/paired", strings.NewReader(`{}`)))
	if w.Code != 200 {
		t.Fatalf("busy desktop refused the list: %d", w.Code)
	}
	s.pairedSlots <- struct{}{}
	w = httptest.NewRecorder()
	s.ServeHTTP(w, httptest.NewRequest("POST", "/paired", strings.NewReader(`{}`)))
	if w.Code != 429 {
		t.Fatalf("second list at once: %d", w.Code)
	}
	<-s.pairedSlots
}

func TestPairedStateFileChecks(t *testing.T) {
	s := testService(t)
	ask := func() int {
		w := httptest.NewRecorder()
		s.ServeHTTP(w, httptest.NewRequest("POST", "/paired", strings.NewReader(`{}`)))
		if w.Code != 200 && !strings.Contains(w.Body.String(), `"ok":false`) {
			t.Fatalf("reply %s", w.Body.String())
		}
		return w.Code
	}
	s.statePath = filepath.Join(s.dir, "absent.json")
	if code := ask(); code != 503 {
		t.Fatalf("absent file: %d", code)
	}
	real := filepath.Join(s.dir, "real.json")
	if err := os.WriteFile(real, stateFile(t, `""`), 0600); err != nil {
		t.Fatal(err)
	}
	s.statePath = filepath.Join(s.dir, "link.json")
	if err := os.Symlink(real, s.statePath); err != nil {
		t.Fatal(err)
	}
	if code := ask(); code != 503 {
		t.Fatalf("symlink: %d", code)
	}
	s.statePath = s.dir
	if code := ask(); code != 503 {
		t.Fatalf("directory: %d", code)
	}
	big := filepath.Join(s.dir, "big.json")
	if err := os.WriteFile(big, append(stateFile(t, `""`), make([]byte, maxStateBytes)...), 0600); err != nil {
		t.Fatal(err)
	}
	s.statePath = big
	if code := ask(); code != 503 {
		t.Fatalf("oversized: %d", code)
	}
	s.statePath = real
	if code := ask(); code != 200 {
		t.Fatalf("empty list: %d", code)
	}
}
