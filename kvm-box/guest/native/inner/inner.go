//go:build linux

// Inner desktop gateway for the measured native desktop profile.
//
// This unprivileged process (UID/EUID 1003) listens on 127.0.0.1:18080. The
// outer native companion gateway (UID 1002) is the only intended client. TCP
// carries no peer identity, so nothing here authenticates a caller: the
// measured nftables policy admits only UID 1002 to port 18080 and binds each
// service port dialled here to its fixed UID. Owner authentication lives only
// in the Wasm controller, and no owner secret is loaded here. See notes.md.
package main

import (
	"bufio"
	"bytes"
	"context"
	"crypto/sha1"
	"crypto/tls"
	"encoding/base64"
	"encoding/binary"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"io/fs"
	"log"
	"mime"
	"net"
	"net/http"
	"os"
	"os/signal"
	"path/filepath"
	"strconv"
	"strings"
	"sync"
	"sync/atomic"
	"syscall"
	"time"
	"unicode"
	"unicode/utf8"
)

// Production endpoints and limits. None of these are configurable at runtime.
const (
	listenAddr = "127.0.0.1:18080"
	innerUID   = 1003

	novncRoot   = "/usr/share/kvm-box/native-ui/novnc"
	nativeUIDir = "/usr/share/kvm-box/native-ui"
	nativePage  = "native.html"
	staticOwner = 0

	computeSocket = "/run/shield/compute.sock"
	computeUID    = 1001
	computeGID    = 1000

	websockifyAddr  = "127.0.0.1:6080"
	tunnelHost      = "127.0.0.1"
	sunshineAPIAddr = "127.0.0.1:47990"
	sunshinePinURL  = "https://" + sunshineAPIAddr + "/api/pin"
	sunshineUser    = "owner"
	credentialPath  = "/run/native-ui/sunshine-password"

	maxActive          = 32
	maxUpgrades        = 32
	maxHeaderBytes     = 16 << 10
	maxBodyBytes       = 8 << 20
	maxStaticFile      = 16 << 20
	maxComputeBody     = 64 << 10
	maxComputeRequest  = 8 << 20
	maxComputeResponse = 16 << 20
	maxPairBody        = 1024
	maxPairList        = 4096
	maxPairReply       = 4096
	maxCredential      = 128
	maxPairingID       = 128
	maxPairName        = 80
	maxUDPPayload      = 65507
	maxJSONDepth       = 64

	readHeaderTimeout  = 5 * time.Second
	bodyReadTimeout    = 30 * time.Second
	writeTimeout       = 60 * time.Second
	idleTimeout        = 60 * time.Second
	deadlineSlack      = 10 * time.Second
	dialTimeout        = 3 * time.Second
	statusTimeout      = 5 * time.Second
	workTimeout        = 21 * time.Minute
	pairTimeout        = 30 * time.Second
	handshakeTimeout   = 10 * time.Second
	wsIdleTimeout      = 300 * time.Second
	wsWriteTimeout     = 30 * time.Second
	tunnelIdleTimeout  = 90 * time.Second
	tunnelWriteTimeout = 10 * time.Second

	tunnelProtocol  = "kvm-box-tunnel"
	tunnelSwitch    = "HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: " + tunnelProtocol + "\r\n\r\n"
	wsGUID          = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11"
	defaultPairName = "KVM Box owner"
	pairCountBody   = `{"status":false,"error":"Start pairing in one Moonlight client, then submit its PIN."}`

	// ST_RDONLY in statfs(2) f_flags.
	stRdonly = 0x1
)

var (
	errNotFound           = errors.New("not found")
	errIntegrity          = errors.New("static tree integrity")
	errComputeUnavailable = errors.New("compute unavailable")
	errComputeResponse    = errors.New("invalid compute response")
	errCredential         = errors.New("pairing credential unavailable")
	errPath               = errors.New("endpoint path refused")
	errFrame              = errors.New("tunnel framing")
)

// config exists so tests can substitute temporary trees, sockets, listeners
// and their own UID. Production always uses productionConfig.
type config struct {
	automationSocket  string
	automationUID     uint32
	automationGID     uint32
	automationPeerUID uint32
	automationTimeout time.Duration
	// Static trees. Every directory from staticAnchor down to the tree root,
	// and every component inside it, must satisfy the ownership checks.
	staticAnchor    string
	novncRoot       string
	nativeDir       string
	staticUID       uint32
	requireReadOnly bool

	computeSocket  string
	computeDirUID  uint32
	computeSockUID uint32
	computeGID     uint32
	computePeerUID uint32
	statusTimeout  time.Duration
	workTimeout    time.Duration

	wsAddr       string
	tunnelTarget func(transport, port string) string

	sunshineAddr string
	credPath     string
	credUID      uint32
	credGID      uint32
	pairTimeout  time.Duration

	dial        time.Duration
	handshake   time.Duration
	wsIdle      time.Duration
	wsWrite     time.Duration
	tunnelIdle  time.Duration
	tunnelWrite time.Duration
	maxActive   int
	maxUpgrades int
}

func productionConfig() config {
	return config{
		automationSocket:  "/run/native-automation/desktop.sock",
		automationUID:     1000,
		automationGID:     1000,
		automationPeerUID: 1000,
		automationTimeout: 125 * time.Second,
		staticAnchor:      "/",
		novncRoot:         novncRoot,
		nativeDir:         nativeUIDir,
		staticUID:         staticOwner,
		requireReadOnly:   true,

		computeSocket:  computeSocket,
		computeDirUID:  computeUID,
		computeSockUID: computeUID,
		computeGID:     computeGID,
		computePeerUID: computeUID,
		statusTimeout:  statusTimeout,
		workTimeout:    workTimeout,

		wsAddr:       websockifyAddr,
		tunnelTarget: func(_, port string) string { return net.JoinHostPort(tunnelHost, port) },

		sunshineAddr: sunshineAPIAddr,
		credPath:     credentialPath,
		credUID:      innerUID,
		credGID:      innerUID,
		pairTimeout:  pairTimeout,

		dial:        dialTimeout,
		handshake:   handshakeTimeout,
		wsIdle:      wsIdleTimeout,
		wsWrite:     wsWriteTimeout,
		tunnelIdle:  tunnelIdleTimeout,
		tunnelWrite: tunnelWriteTimeout,
		maxActive:   maxActive,
		maxUpgrades: maxUpgrades,
	}
}

func main() {
	if err := run(); err != nil {
		fmt.Fprintln(os.Stderr, "native inner gateway:", err)
		os.Exit(1)
	}
}

func run() error {
	if err := startupCheck(os.Getuid(), os.Geteuid(), os.LookupEnv, os.Args); err != nil {
		return err
	}
	g := newGateway(productionConfig())
	ln, err := net.Listen("tcp", listenAddr)
	if err != nil {
		return errors.New("cannot listen")
	}
	sig := make(chan os.Signal, 1)
	signal.Notify(sig, syscall.SIGTERM, syscall.SIGINT)
	go func() {
		<-sig
		g.Close()
	}()
	if err := g.Serve(ln); err != nil && !errors.Is(err, http.ErrServerClosed) {
		return errors.New("serve failed")
	}
	return nil
}

// startupCheck refuses the wrong identity, any arguments, and inherited owner
// credential variables. The launcher must supply a clean environment.
func startupCheck(uid, euid int, lookup func(string) (string, bool), args []string) error {
	if len(args) > 1 {
		return errors.New("arguments are not accepted")
	}
	if uid != innerUID || euid != innerUID {
		return errors.New("must run as uid 1003")
	}
	for _, k := range []string{"KVMBOX_OWNER_PASSWORD", "KVMBOX_ACCESS_SHA256", "KVMBOX_AUTOMATION_KEY"} {
		if _, ok := lookup(k); ok {
			return errors.New("inherited owner credential variable refused")
		}
	}
	return nil
}

type gateway struct {
	cfg         config
	ctx         context.Context
	cancel      context.CancelFunc
	slots       chan struct{}
	wsSlots     chan struct{}
	tunnelSlots chan struct{}
	pairSem     chan struct{}
	sunshine    *http.Client
	srv         *http.Server
}

func newGateway(cfg config) *gateway {
	ctx, cancel := context.WithCancel(context.Background())
	g := &gateway{
		cfg:         cfg,
		ctx:         ctx,
		cancel:      cancel,
		slots:       make(chan struct{}, cfg.maxActive),
		wsSlots:     make(chan struct{}, cfg.maxUpgrades),
		tunnelSlots: make(chan struct{}, cfg.maxUpgrades),
		pairSem:     make(chan struct{}, 1),
	}
	dialer := &net.Dialer{Timeout: cfg.dial}
	g.sunshine = &http.Client{
		Transport: &http.Transport{
			Proxy: nil,
			// The destination is fixed regardless of the request URL.
			DialContext: func(ctx context.Context, _, _ string) (net.Conn, error) {
				return dialer.DialContext(ctx, "tcp", cfg.sunshineAddr)
			},
			// Sunshine's certificate is self-signed and generated by the
			// desktop UID itself, so it cannot be pinned meaningfully here.
			// Endpoint identity comes from the measured nft rule that only
			// UID 1000 may source TCP 47990. See notes.md.
			TLSClientConfig:        &tls.Config{InsecureSkipVerify: true, MinVersion: tls.VersionTLS12},
			TLSHandshakeTimeout:    cfg.dial,
			DisableKeepAlives:      true,
			DisableCompression:     true,
			MaxResponseHeaderBytes: maxHeaderBytes,
		},
		CheckRedirect: func(*http.Request, []*http.Request) error { return http.ErrUseLastResponse },
	}
	g.srv = &http.Server{
		Handler:                      g,
		ReadHeaderTimeout:            readHeaderTimeout,
		IdleTimeout:                  idleTimeout,
		MaxHeaderBytes:               maxHeaderBytes,
		DisableGeneralOptionsHandler: true,
		BaseContext:                  func(net.Listener) context.Context { return ctx },
		ErrorLog:                     log.New(io.Discard, "", 0),
	}
	return g
}

// Serve accepts at most cfg.maxActive connections at a time, including
// upgraded streams, which keep their slot until closed.
func (g *gateway) Serve(ln net.Listener) error {
	return g.srv.Serve(&limitListener{Listener: ln, slots: g.slots})
}

// Close stops the server and tears down every hijacked stream.
func (g *gateway) Close() error {
	g.cancel()
	return g.srv.Close()
}

var busyResponse = func() string {
	body := `{"error":"service unavailable"}`
	return "HTTP/1.1 503 Service Unavailable\r\nContent-Type: application/json\r\nX-Content-Type-Options: nosniff\r\n" +
		"Cache-Control: no-store\r\nConnection: close\r\nContent-Length: " + strconv.Itoa(len(body)) + "\r\n\r\n" + body
}()

type limitListener struct {
	net.Listener
	slots chan struct{}
}

func (l *limitListener) Accept() (net.Conn, error) {
	for {
		c, err := l.Listener.Accept()
		if err != nil {
			return nil, err
		}
		select {
		case l.slots <- struct{}{}:
			return &slotConn{Conn: c, release: func() { <-l.slots }}, nil
		default:
			c.SetWriteDeadline(time.Now().Add(100 * time.Millisecond))
			io.WriteString(c, busyResponse)
			c.Close()
		}
	}
}

type slotConn struct {
	net.Conn
	once    sync.Once
	release func()
}

func (c *slotConn) Close() error {
	err := c.Conn.Close()
	c.once.Do(c.release)
	return err
}

func (c *slotConn) CloseWrite() error {
	if cw, ok := c.Conn.(interface{ CloseWrite() error }); ok {
		return cw.CloseWrite()
	}
	return errors.New("half-close unsupported")
}

func acquire(sem chan struct{}) (func(), bool) {
	select {
	case sem <- struct{}{}:
		return func() { <-sem }, true
	default:
		return nil, false
	}
}

// Routing.

type routeKind int

const (
	routeNone routeKind = iota
	routeAutomation
	routeNoVNC
	routeNativePage
	routeComputeStatus
	routeComputeWork
	routePair
	routeWebSocket
	routeTunnel
)

func classify(path string) routeKind {
	switch path {
	case "/exec", "/computer":
		return routeAutomation
	case "/vnc.html":
		return routeNoVNC
	case "/compute/", "/compute/native/":
		return routeNativePage
	case "/compute/api/native":
		return routeComputeStatus
	case "/compute/native/compute":
		return routeComputeWork
	case "/api/moonlight/pair":
		return routePair
	case "/websockify":
		return routeWebSocket
	case "/api/moonlight/tunnel":
		return routeTunnel
	}
	for _, p := range []string{"/app/", "/core/", "/vendor/"} {
		if strings.HasPrefix(path, p) && len(path) > len(p) {
			return routeNoVNC
		}
	}
	return routeNone
}

func (k routeKind) allows(method string) bool {
	switch k {
	case routeNoVNC, routeNativePage:
		return method == http.MethodGet || method == http.MethodHead
	case routeComputeStatus, routeWebSocket, routeTunnel:
		return method == http.MethodGet
	case routeComputeWork, routePair, routeAutomation:
		return method == http.MethodPost
	}
	return false
}

func (g *gateway) ServeHTTP(w http.ResponseWriter, r *http.Request) {
	securityHeaders(w.Header())
	// Bound body reads and response writes per request. Handlers that wait
	// longer (compute work) extend these to their own absolute deadline.
	rc := http.NewResponseController(w)
	now := time.Now()
	rc.SetReadDeadline(now.Add(bodyReadTimeout))
	rc.SetWriteDeadline(now.Add(writeTimeout))

	path, query, hasQuery, code := admit(r)
	if code != 0 {
		writeError(w, code)
		return
	}
	kind := classify(path)
	if kind == routeNone {
		writeError(w, http.StatusNotFound)
		return
	}
	if !kind.allows(r.Method) {
		writeError(w, http.StatusMethodNotAllowed)
		return
	}
	switch kind {
	case routeWebSocket:
		g.serveWebSocket(w, r, hasQuery)
		return
	case routeTunnel:
		g.serveTunnel(w, r, query, hasQuery)
		return
	}
	if len(r.Header.Values("Upgrade")) > 0 || connectionHasUpgrade(r.Header) {
		writeError(w, http.StatusBadRequest)
		return
	}
	// Static pages ignore a well-formed query (noVNC reads its own settings
	// from it). API routes take none.
	if hasQuery && kind != routeNoVNC && kind != routeNativePage {
		writeError(w, http.StatusBadRequest)
		return
	}
	switch kind {
	case routeAutomation:
		g.serveAutomation(w, r)
	case routeNoVNC:
		g.serveStatic(w, r, g.cfg.novncRoot, path[1:])
	case routeNativePage:
		g.serveStatic(w, r, g.cfg.nativeDir, nativePage)
	case routeComputeStatus:
		g.serveComputeStatus(w, r)
	case routeComputeWork:
		g.serveComputeWork(w, r)
	case routePair:
		g.servePair(w, r)
	}
}

var singletonHeaders = []string{
	"Content-Length", "Content-Type", "Upgrade", "Connection", "Origin", "Expect",
	"X-Kvm-Box-Request", "Sec-Websocket-Key", "Sec-Websocket-Version", "Sec-Websocket-Protocol",
}

// The outer gateway strips these; their presence here means unexpected
// direct input, so they are refused rather than ignored or reused.
var refusedHeaders = []string{
	"Authorization", "Proxy-Authorization", "Cookie", "Content-Encoding", "Transfer-Encoding", "X-Api-Key",
}

// admit applies the checks common to every route. It returns the raw path
// and query, or a nonzero HTTP status.
func admit(r *http.Request) (path, query string, hasQuery bool, code int) {
	switch r.Method {
	case http.MethodGet, http.MethodHead, http.MethodPost:
	default:
		return "", "", false, http.StatusMethodNotAllowed
	}
	if r.ProtoMajor != 1 || r.ProtoMinor != 1 {
		return "", "", false, http.StatusBadRequest
	}
	if r.Host != listenAddr {
		return "", "", false, http.StatusBadRequest
	}
	if len(r.TransferEncoding) > 0 {
		return "", "", false, http.StatusBadRequest
	}
	if requestHeaderBytes(r) > maxHeaderBytes {
		return "", "", false, http.StatusRequestHeaderFieldsTooLarge
	}
	for _, k := range refusedHeaders {
		if len(r.Header.Values(k)) > 0 {
			return "", "", false, http.StatusBadRequest
		}
	}
	for _, k := range singletonHeaders {
		if len(r.Header.Values(k)) > 1 {
			return "", "", false, http.StatusBadRequest
		}
	}
	if r.ContentLength < 0 {
		return "", "", false, http.StatusBadRequest
	}
	if r.ContentLength > maxBodyBytes {
		return "", "", false, http.StatusRequestEntityTooLarge
	}
	if (r.Method == http.MethodGet || r.Method == http.MethodHead) && r.ContentLength != 0 {
		return "", "", false, http.StatusBadRequest
	}
	// Origin-form only: rejects absolute URLs, authority form and "*".
	if !strings.HasPrefix(r.RequestURI, "/") {
		return "", "", false, http.StatusBadRequest
	}
	path, query, hasQuery = strings.Cut(r.RequestURI, "?")
	if !validPath(path) || path != r.URL.Path || r.URL.RawPath != "" {
		return "", "", false, http.StatusBadRequest
	}
	if hasQuery && !validQuery(query) {
		return "", "", false, http.StatusBadRequest
	}
	return path, query, hasQuery, 0
}

func requestHeaderBytes(r *http.Request) int {
	n := len(r.Method) + len(r.RequestURI) + len(r.Proto) + 4 + len("Host: ") + len(r.Host) + 2
	for k, vs := range r.Header {
		for _, v := range vs {
			n += len(k) + len(v) + 4
		}
	}
	return n
}

func isAlnum(c byte) bool {
	return c >= 'a' && c <= 'z' || c >= 'A' && c <= 'Z' || c >= '0' && c <= '9'
}

func isUnreserved(c byte) bool {
	return isAlnum(c) || c == '-' || c == '.' || c == '_' || c == '~'
}

func isHex(c byte) bool {
	return c >= '0' && c <= '9' || c >= 'a' && c <= 'f' || c >= 'A' && c <= 'F'
}

// validPath accepts only unreserved characters and '/', so the raw and
// decoded paths are identical. Percent-encoding, backslashes, dot segments
// and empty segments (except a trailing slash) are refused.
func validPath(p string) bool {
	if len(p) < 2 || p[0] != '/' {
		return false
	}
	for i := 0; i < len(p); i++ {
		if !isUnreserved(p[i]) && p[i] != '/' {
			return false
		}
	}
	segs := strings.Split(p[1:], "/")
	for i, s := range segs {
		if s == "." || s == ".." || s == "" && i != len(segs)-1 {
			return false
		}
	}
	return true
}

func validQuery(q string) bool {
	if len(q) > 2048 {
		return false
	}
	for i := 0; i < len(q); i++ {
		c := q[i]
		switch {
		case isUnreserved(c), strings.IndexByte("&=+,:@/?!$'()*", c) >= 0:
		case c == '%':
			if i+2 >= len(q) || !isHex(q[i+1]) || !isHex(q[i+2]) {
				return false
			}
			i += 2
		default:
			return false
		}
	}
	return true
}

func connectionTokens(h http.Header) []string {
	var out []string
	for _, v := range h.Values("Connection") {
		for _, t := range strings.Split(v, ",") {
			out = append(out, strings.ToLower(strings.TrimSpace(t)))
		}
	}
	return out
}

func connectionHasUpgrade(h http.Header) bool {
	for _, t := range connectionTokens(h) {
		if t == "upgrade" {
			return true
		}
	}
	return false
}

// connectionIsUpgrade requires one Connection header listing only
// "upgrade" and optionally "keep-alive".
func connectionIsUpgrade(h http.Header) bool {
	if len(h.Values("Connection")) != 1 {
		return false
	}
	found := false
	for _, t := range connectionTokens(h) {
		switch t {
		case "upgrade":
			found = true
		case "keep-alive":
		default:
			return false
		}
	}
	return found
}

func upgradeIs(h http.Header, proto string, fold bool) bool {
	vs := h.Values("Upgrade")
	if len(vs) != 1 {
		return false
	}
	if fold {
		return strings.EqualFold(vs[0], proto)
	}
	return vs[0] == proto
}

func securityHeaders(h http.Header) {
	h.Set("X-Content-Type-Options", "nosniff")
	h.Set("Cache-Control", "no-store")
	h.Set("Content-Security-Policy", "frame-ancestors 'self'")
	h.Set("X-Frame-Options", "SAMEORIGIN")
	h.Set("Referrer-Policy", "no-referrer")
}

func writeJSON(w http.ResponseWriter, code int, body []byte) {
	h := w.Header()
	h.Set("Content-Type", "application/json")
	h.Set("Content-Length", strconv.Itoa(len(body)))
	w.WriteHeader(code)
	w.Write(body)
}

// writeError emits only fixed text derived from the status code.
func writeError(w http.ResponseWriter, code int) {
	writeJSON(w, code, []byte(`{"error":"`+strings.ToLower(http.StatusText(code))+`"}`))
}

func jsonContentType(h http.Header) bool {
	vs := h.Values("Content-Type")
	if len(vs) != 1 {
		return false
	}
	mt, params, err := mime.ParseMediaType(vs[0])
	if err != nil || mt != "application/json" {
		return false
	}
	switch len(params) {
	case 0:
		return true
	case 1:
		return strings.EqualFold(params["charset"], "utf-8")
	}
	return false
}

// readBody reads exactly the declared Content-Length, at most limit bytes.
func readBody(r *http.Request, limit int64) ([]byte, int) {
	if r.ContentLength > limit {
		return nil, http.StatusRequestEntityTooLarge
	}
	if r.ContentLength <= 0 {
		return nil, http.StatusBadRequest
	}
	b, err := io.ReadAll(io.LimitReader(r.Body, limit+1))
	if err != nil || int64(len(b)) != r.ContentLength {
		return nil, http.StatusBadRequest
	}
	return b, 0
}

// Static assets.

var staticTypes = map[string]string{
	".html":  "text/html; charset=utf-8",
	".js":    "text/javascript; charset=utf-8",
	".mjs":   "text/javascript; charset=utf-8",
	".css":   "text/css; charset=utf-8",
	".json":  "application/json",
	".png":   "image/png",
	".jpg":   "image/jpeg",
	".jpeg":  "image/jpeg",
	".gif":   "image/gif",
	".webp":  "image/webp",
	".ico":   "image/x-icon",
	".svg":   "image/svg+xml",
	".woff":  "font/woff",
	".woff2": "font/woff2",
	".ttf":   "font/ttf",
	".otf":   "font/otf",
	".mp3":   "audio/mpeg",
	".oga":   "audio/ogg",
}

// staticType maps an allowlisted, case-exact extension to its MIME type.
// Hidden names and empty segments (directory requests) are refused.
func staticType(rel string) (string, bool) {
	segs := strings.Split(rel, "/")
	for _, s := range segs {
		if s == "" || s[0] == '.' {
			return "", false
		}
	}
	name := segs[len(segs)-1]
	dot := strings.LastIndexByte(name, '.')
	if dot <= 0 {
		return "", false
	}
	t, ok := staticTypes[name[dot:]]
	return t, ok
}

func (g *gateway) serveStatic(w http.ResponseWriter, r *http.Request, dir, rel string) {
	ctype, ok := staticType(rel)
	if !ok {
		writeError(w, http.StatusNotFound)
		return
	}
	f, size, err := g.openStatic(dir, rel)
	if err != nil {
		if errors.Is(err, errNotFound) {
			writeError(w, http.StatusNotFound)
		} else {
			writeError(w, http.StatusServiceUnavailable)
		}
		return
	}
	defer f.Close()
	h := w.Header()
	h.Set("Content-Type", ctype)
	h.Set("Content-Length", strconv.FormatInt(size, 10))
	w.WriteHeader(http.StatusOK)
	if r.Method == http.MethodHead {
		return
	}
	io.CopyN(w, f, size)
}

// trusted requires the fixed owner, no group/world write and no set-ID bits.
func (g *gateway) trusted(fi fs.FileInfo) bool {
	st, ok := fi.Sys().(*syscall.Stat_t)
	return ok && st.Uid == g.cfg.staticUID && fi.Mode().Perm()&0o022 == 0 &&
		fi.Mode()&(fs.ModeSetuid|fs.ModeSetgid) == 0
}

// checkStaticAncestors walks from the anchor to dir without following
// symlinks, requiring every directory to be trusted.
func (g *gateway) checkStaticAncestors(dir string) error {
	anchor := g.cfg.staticAnchor
	if !filepath.IsAbs(dir) || filepath.Clean(dir) != dir || !filepath.IsAbs(anchor) {
		return errIntegrity
	}
	rel, err := filepath.Rel(anchor, dir)
	if err != nil || rel == ".." || strings.HasPrefix(rel, "../") {
		return errIntegrity
	}
	var parts []string
	if rel != "." {
		parts = strings.Split(rel, "/")
	}
	p := anchor
	for i := -1; i < len(parts); i++ {
		if i >= 0 {
			p = filepath.Join(p, parts[i])
		}
		fi, err := os.Lstat(p)
		if err != nil || !fi.IsDir() || !g.trusted(fi) {
			return errIntegrity
		}
	}
	return nil
}

// openStatic opens rel beneath dir through an os.Root, so no lookup can
// leave the tree. Symlinks anywhere inside the tree are not served (the
// production tree is flattened). The opened inode must be the one checked.
func (g *gateway) openStatic(dir, rel string) (*os.File, int64, error) {
	if err := g.checkStaticAncestors(dir); err != nil {
		return nil, 0, err
	}
	root, err := os.OpenRoot(dir)
	if err != nil {
		return nil, 0, errIntegrity
	}
	defer root.Close()
	segs := strings.Split(rel, "/")
	for i := 1; i < len(segs); i++ {
		fi, err := root.Lstat(strings.Join(segs[:i], "/"))
		if err != nil {
			if errors.Is(err, fs.ErrNotExist) {
				return nil, 0, errNotFound
			}
			return nil, 0, errIntegrity
		}
		if !fi.IsDir() {
			return nil, 0, errNotFound
		}
		if !g.trusted(fi) {
			return nil, 0, errIntegrity
		}
	}
	fi, err := root.Lstat(rel)
	if err != nil {
		if errors.Is(err, fs.ErrNotExist) {
			return nil, 0, errNotFound
		}
		return nil, 0, errIntegrity
	}
	if !fi.Mode().IsRegular() {
		return nil, 0, errNotFound
	}
	if !g.trusted(fi) || fi.Size() > maxStaticFile {
		return nil, 0, errIntegrity
	}
	f, err := root.Open(rel)
	if err != nil {
		return nil, 0, errIntegrity
	}
	opened, err := f.Stat()
	if err != nil || !os.SameFile(fi, opened) || !opened.Mode().IsRegular() || !g.trusted(opened) ||
		opened.Size() > maxStaticFile || g.cfg.requireReadOnly && !readOnlyFS(f) {
		f.Close()
		return nil, 0, errIntegrity
	}
	return f, opened.Size(), nil
}

func readOnlyFS(f *os.File) bool {
	raw, err := f.SyscallConn()
	if err != nil {
		return false
	}
	var st syscall.Statfs_t
	var serr error
	if err := raw.Control(func(fd uintptr) { serr = syscall.Fstatfs(int(fd), &st) }); err != nil || serr != nil {
		return false
	}
	return st.Flags&stRdonly != 0
}

// Native compute over the fixed Unix socket.

func (g *gateway) serveComputeStatus(w http.ResponseWriter, r *http.Request) {
	deadline := time.Now().Add(g.cfg.statusTimeout)
	body, err := g.computeExchange(r.Context(), []byte(`{"op":"status"}`), deadline)
	writeCompute(w, body, err)
}

func (g *gateway) serveComputeWork(w http.ResponseWriter, r *http.Request) {
	if !jsonContentType(r.Header) {
		writeError(w, http.StatusUnsupportedMediaType)
		return
	}
	body, code := readBody(r, maxComputeBody)
	if code != 0 {
		writeError(w, code)
		return
	}
	if !validComputeRequest(body) {
		writeError(w, http.StatusBadRequest)
		return
	}
	deadline := time.Now().Add(g.cfg.workTimeout)
	// The request body is fully read; the server's background read and the
	// eventual response must outlive the per-request defaults.
	rc := http.NewResponseController(w)
	rc.SetReadDeadline(deadline.Add(deadlineSlack))
	rc.SetWriteDeadline(deadline.Add(deadlineSlack))
	resp, err := g.computeExchange(r.Context(), body, deadline)
	writeCompute(w, resp, err)
}

func writeCompute(w http.ResponseWriter, body []byte, err error) {
	switch {
	case err == nil:
		writeJSON(w, http.StatusOK, body)
	case errors.Is(err, errComputeResponse):
		writeError(w, http.StatusBadGateway)
	default:
		writeError(w, http.StatusServiceUnavailable)
	}
}

// validComputeRequest requires a strict JSON object whose op is llama or sd.
// All other fields are left to the compute service; the body is forwarded
// byte for byte.
func validComputeRequest(b []byte) bool {
	top, ok := strictObject(b, maxJSONDepth)
	if !ok {
		return false
	}
	op, _ := top["op"].(string)
	return op == "llama" || op == "sd"
}

func validComputeResponse(b []byte) bool {
	return len(b) >= 2 && b[0] == '{' && b[len(b)-1] == '}' && utf8.Valid(b) && json.Valid(b)
}

// computeExchange performs one framed request on a fresh connection. The
// socket path is checked before connecting and the server's SO_PEERCRED UID
// before anything is written; there is no fallback. One absolute deadline
// covers connect, write and read, so slow progress cannot extend it.
func (g *gateway) computeExchange(parent context.Context, req []byte, deadline time.Time) ([]byte, error) {
	ctx, cancel := context.WithDeadline(parent, deadline)
	defer cancel()
	if len(req) == 0 || len(req) > maxComputeRequest || ctx.Err() != nil {
		return nil, errComputeUnavailable
	}
	if err := checkSocketPath(g.cfg.computeSocket, g.cfg.computeDirUID, g.cfg.computeSockUID, g.cfg.computeGID); err != nil {
		return nil, errComputeUnavailable
	}
	d := net.Dialer{Timeout: g.cfg.dial}
	c, err := d.DialContext(ctx, "unix", g.cfg.computeSocket)
	if err != nil {
		return nil, errComputeUnavailable
	}
	defer c.Close()
	uc, ok := c.(*net.UnixConn)
	if !ok {
		return nil, errComputeUnavailable
	}
	// Install the absolute deadline before the cancellation callback, so a
	// later reset can never overwrite cancellation's expired deadline.
	dl, _ := ctx.Deadline()
	uc.SetDeadline(dl)
	stop := context.AfterFunc(ctx, func() { uc.SetDeadline(time.Unix(1, 0)) })
	defer stop()
	if uid, err := unixPeerUID(uc); err != nil || uid != g.cfg.computePeerUID {
		return nil, errComputeUnavailable
	}
	frame := make([]byte, 4+len(req))
	binary.BigEndian.PutUint32(frame, uint32(len(req)))
	copy(frame[4:], req)
	if _, err := uc.Write(frame); err != nil {
		return nil, errComputeUnavailable
	}
	if err := uc.CloseWrite(); err != nil {
		return nil, errComputeUnavailable
	}
	var hdr [4]byte
	if _, err := io.ReadFull(uc, hdr[:]); err != nil {
		if err == io.EOF {
			// Closed before replying, e.g. peer UID not admitted.
			return nil, errComputeUnavailable
		}
		return nil, readFailure(ctx, err)
	}
	n := binary.BigEndian.Uint32(hdr[:])
	if n < 1 || n > maxComputeResponse {
		return nil, errComputeResponse
	}
	body := make([]byte, n)
	if _, err := io.ReadFull(uc, body); err != nil {
		return nil, readFailure(ctx, err)
	}
	var extra [1]byte
	if _, err := io.ReadFull(uc, extra[:]); err != io.EOF {
		if err != nil {
			return nil, readFailure(ctx, err)
		}
		return nil, errComputeResponse
	}
	if ctx.Err() != nil {
		return nil, errComputeUnavailable
	}
	if !validComputeResponse(body) {
		return nil, errComputeResponse
	}
	return body, nil
}

func readFailure(ctx context.Context, err error) error {
	if ctx.Err() != nil || isTimeout(err) {
		return errComputeUnavailable
	}
	return errComputeResponse
}

// realDir requires dir to be an absolute, clean, symlink-free path naming a
// directory with exactly the given owner, group and permission bits.
func realDir(dir string, uid, gid uint32, perm fs.FileMode) error {
	if !filepath.IsAbs(dir) || filepath.Clean(dir) != dir {
		return errPath
	}
	if real, err := filepath.EvalSymlinks(dir); err != nil || real != dir {
		return errPath
	}
	fi, err := os.Lstat(dir)
	if err != nil {
		return errPath
	}
	st, ok := fi.Sys().(*syscall.Stat_t)
	if !ok || fi.Mode() != fs.ModeDir|perm || st.Uid != uid || st.Gid != gid {
		return errPath
	}
	return nil
}

// checkSocketPath requires the parent directory dirUID:gid 0750 and the
// socket sockUID:gid 0660. Lstat refuses a symlinked socket.
func checkSocketPath(path string, dirUID, sockUID, gid uint32) error {
	if err := realDir(filepath.Dir(path), dirUID, gid, 0o750); err != nil {
		return err
	}
	si, err := os.Lstat(path)
	if err != nil {
		return errPath
	}
	ss, ok := si.Sys().(*syscall.Stat_t)
	if !ok || si.Mode() != fs.ModeSocket|0o660 || ss.Uid != sockUID || ss.Gid != gid {
		return errPath
	}
	return nil
}

func unixPeerUID(c *net.UnixConn) (uint32, error) {
	raw, err := c.SyscallConn()
	if err != nil {
		return 0, err
	}
	var cred *syscall.Ucred
	var serr error
	if err := raw.Control(func(fd uintptr) {
		cred, serr = syscall.GetsockoptUcred(int(fd), syscall.SOL_SOCKET, syscall.SO_PEERCRED)
	}); err != nil {
		return 0, err
	}
	if serr != nil {
		return 0, serr
	}
	return cred.Uid, nil
}

// Strict JSON.

type jsonNested struct{}

// strictObject decodes b as exactly one JSON object with valid UTF-8, no
// duplicate member names at any depth, nesting at most maxDepth and no
// trailing data. It returns the top-level members: scalars as decoder tokens
// (string, json.Number, bool, nil), nested values as jsonNested.
func strictObject(b []byte, maxDepth int) (map[string]any, bool) {
	if !utf8.Valid(b) {
		return nil, false
	}
	dec := json.NewDecoder(bytes.NewReader(b))
	dec.UseNumber()
	if tok, err := dec.Token(); err != nil || tok != json.Delim('{') {
		return nil, false
	}
	top := map[string]any{}
	for dec.More() {
		k, err := dec.Token()
		key, ok := k.(string)
		if err != nil || !ok {
			return nil, false
		}
		if _, dup := top[key]; dup {
			return nil, false
		}
		v, ok := strictValue(dec, 1, maxDepth)
		if !ok {
			return nil, false
		}
		top[key] = v
	}
	if tok, err := dec.Token(); err != nil || tok != json.Delim('}') {
		return nil, false
	}
	if _, err := dec.Token(); err != io.EOF {
		return nil, false
	}
	return top, true
}

func strictValue(dec *json.Decoder, depth, maxDepth int) (any, bool) {
	tok, err := dec.Token()
	if err != nil {
		return nil, false
	}
	d, ok := tok.(json.Delim)
	if !ok {
		return tok, true
	}
	if depth >= maxDepth {
		return nil, false
	}
	switch d {
	case '{':
		seen := map[string]bool{}
		for dec.More() {
			k, err := dec.Token()
			key, ok := k.(string)
			if err != nil || !ok || seen[key] {
				return nil, false
			}
			seen[key] = true
			if _, ok := strictValue(dec, depth+1, maxDepth); !ok {
				return nil, false
			}
		}
	case '[':
		for dec.More() {
			if _, ok := strictValue(dec, depth+1, maxDepth); !ok {
				return nil, false
			}
		}
	default:
		return nil, false
	}
	if _, err := dec.Token(); err != nil {
		return nil, false
	}
	return jsonNested{}, true
}

// decodeOne decodes exactly one JSON value into v with no trailing data.
func decodeOne(b []byte, v any) bool {
	if !utf8.Valid(b) {
		return false
	}
	dec := json.NewDecoder(bytes.NewReader(b))
	if err := dec.Decode(v); err != nil {
		return false
	}
	_, err := dec.Token()
	return err == io.EOF
}

// Upgraded streams.

// handshakeReader bounds the upstream 101 response head.
type handshakeReader struct {
	r io.Reader
	n int64
}

func (h *handshakeReader) Read(p []byte) (int, error) {
	if h.n <= 0 {
		return 0, errors.New("handshake too large")
	}
	if int64(len(p)) > h.n {
		p = p[:h.n]
	}
	n, err := h.r.Read(p)
	h.n -= int64(n)
	return n, err
}

// drain returns a reader yielding br's buffered bytes followed by conn.
func drain(br *bufio.Reader, conn net.Conn) io.Reader {
	n := br.Buffered()
	if n == 0 {
		return conn
	}
	peek, _ := br.Peek(n)
	return io.MultiReader(bytes.NewReader(append([]byte(nil), peek...)), conn)
}

func wsAccept(key string) string {
	sum := sha1.Sum([]byte(key + wsGUID))
	return base64.StdEncoding.EncodeToString(sum[:])
}

func validWebSocketRequest(h http.Header) bool {
	if v := h.Values("Sec-Websocket-Version"); len(v) != 1 || v[0] != "13" {
		return false
	}
	k := h.Values("Sec-Websocket-Key")
	if len(k) != 1 || len(k[0]) != 24 {
		return false
	}
	raw, err := base64.StdEncoding.DecodeString(k[0])
	if err != nil || len(raw) != 16 || base64.StdEncoding.EncodeToString(raw) != k[0] {
		return false
	}
	if p := h.Values("Sec-Websocket-Protocol"); len(p) > 1 || len(p) == 1 && p[0] != "binary" {
		return false
	}
	return true
}

// hijack takes over the client connection and writes the fixed 101 head.
// It returns the connection and a reader that preserves buffered early bytes.
func hijack(w http.ResponseWriter, head string, write time.Duration) (net.Conn, io.Reader, bool) {
	hj, ok := w.(http.Hijacker)
	if !ok {
		writeError(w, http.StatusInternalServerError)
		return nil, nil, false
	}
	c, brw, err := hj.Hijack()
	if err != nil {
		return nil, nil, false
	}
	c.SetDeadline(time.Time{})
	c.SetWriteDeadline(time.Now().Add(write))
	if _, err := io.WriteString(c, head); err != nil {
		c.Close()
		return nil, nil, false
	}
	c.SetWriteDeadline(time.Time{})
	return c, drain(brw.Reader, c), true
}

// serveWebSocket proxies a strictly validated WebSocket upgrade to the fixed
// websockify endpoint. Only key, version and the optional "binary"
// subprotocol are forwarded. The upstream 101 is validated before hijacking.
func (g *gateway) serveWebSocket(w http.ResponseWriter, r *http.Request, hasQuery bool) {
	if hasQuery || r.ContentLength != 0 || !connectionIsUpgrade(r.Header) ||
		!upgradeIs(r.Header, "websocket", true) || !validWebSocketRequest(r.Header) {
		writeError(w, http.StatusBadRequest)
		return
	}
	release, ok := acquire(g.wsSlots)
	if !ok {
		writeError(w, http.StatusTooManyRequests)
		return
	}
	defer release()
	key := r.Header.Get("Sec-Websocket-Key")
	offered := r.Header.Get("Sec-Websocket-Protocol") == "binary"
	head := "GET /websockify HTTP/1.1\r\nHost: " + g.cfg.wsAddr + "\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n" +
		"Sec-WebSocket-Version: 13\r\nSec-WebSocket-Key: " + key + "\r\n"
	if offered {
		head += "Sec-WebSocket-Protocol: binary\r\n"
	}
	head += "\r\n"

	d := net.Dialer{Timeout: g.cfg.dial}
	up, err := d.DialContext(r.Context(), "tcp", g.cfg.wsAddr)
	if err != nil {
		writeError(w, http.StatusServiceUnavailable)
		return
	}
	handedOff := false
	defer func() {
		if !handedOff {
			up.Close()
		}
	}()
	stop := context.AfterFunc(r.Context(), func() { up.Close() })
	defer stop()
	up.SetDeadline(time.Now().Add(g.cfg.handshake))
	if _, err := io.WriteString(up, head); err != nil {
		writeError(w, http.StatusBadGateway)
		return
	}
	ubr := bufio.NewReaderSize(&handshakeReader{r: up, n: maxHeaderBytes}, 4096)
	resp, err := http.ReadResponse(ubr, &http.Request{Method: http.MethodGet})
	if err != nil || resp.StatusCode != http.StatusSwitchingProtocols ||
		!connectionIsUpgrade(resp.Header) || !upgradeIs(resp.Header, "websocket", true) {
		writeError(w, http.StatusBadGateway)
		return
	}
	accept := wsAccept(key)
	if acc := resp.Header.Values("Sec-Websocket-Accept"); len(acc) != 1 || acc[0] != accept {
		writeError(w, http.StatusBadGateway)
		return
	}
	if len(resp.Header.Values("Sec-Websocket-Extensions")) > 0 {
		writeError(w, http.StatusBadGateway)
		return
	}
	proto := resp.Header.Values("Sec-Websocket-Protocol")
	if len(proto) > 1 || len(proto) == 1 && (proto[0] != "binary" || !offered) {
		writeError(w, http.StatusBadGateway)
		return
	}
	out := "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: " + accept + "\r\n"
	if len(proto) == 1 {
		out += "Sec-WebSocket-Protocol: binary\r\n"
	}
	out += "\r\n"
	client, fromClient, ok := hijack(w, out, g.cfg.wsWrite)
	if !ok {
		return
	}
	handedOff = true
	up.SetDeadline(time.Time{})
	fromUp := drain(ubr, up)
	a := newActivity(g.cfg.wsIdle, g.cfg.wsWrite)
	relay(r.Context(), client, up,
		func() error { return copyHalf(up, client, fromClient, a) },
		func() error { return copyHalf(client, up, fromUp, a) })
}

// Fixed Sunshine destinations, compared as exact canonical strings. TCP 47990
// (the Sunshine admin API) is deliberately absent.
var tunnelPorts = map[string][]string{
	"tcp": {"47984", "47989", "48010"},
	"udp": {"47998", "47999", "48000"},
}

// parseTunnelQuery accepts exactly one port and one transport, in either
// order, with unencoded values naming a fixed Sunshine destination.
func parseTunnelQuery(raw string) (transport, port string, ok bool) {
	parts := strings.Split(raw, "&")
	if len(parts) != 2 {
		return "", "", false
	}
	for _, p := range parts {
		k, v, found := strings.Cut(p, "=")
		if !found || v == "" {
			return "", "", false
		}
		switch {
		case k == "port" && port == "":
			port = v
		case k == "transport" && transport == "":
			transport = v
		default:
			return "", "", false
		}
	}
	for _, p := range tunnelPorts[transport] {
		if p == port {
			return transport, port, true
		}
	}
	return "", "", false
}

// serveTunnel carries the legacy Moonlight tunnel: raw bytes for TCP, and
// u16 big-endian length-prefixed datagrams for UDP.
func (g *gateway) serveTunnel(w http.ResponseWriter, r *http.Request, query string, hasQuery bool) {
	transport, port, ok := parseTunnelQuery(query)
	if !hasQuery || !ok || r.ContentLength != 0 || !connectionIsUpgrade(r.Header) ||
		!upgradeIs(r.Header, tunnelProtocol, false) {
		writeError(w, http.StatusBadRequest)
		return
	}
	release, ok := acquire(g.tunnelSlots)
	if !ok {
		writeError(w, http.StatusTooManyRequests)
		return
	}
	defer release()
	d := net.Dialer{Timeout: g.cfg.dial}
	up, err := d.DialContext(r.Context(), transport, g.cfg.tunnelTarget(transport, port))
	if err != nil {
		writeError(w, http.StatusServiceUnavailable)
		return
	}
	client, fromClient, ok := hijack(w, tunnelSwitch, g.cfg.tunnelWrite)
	if !ok {
		up.Close()
		return
	}
	a := newActivity(g.cfg.tunnelIdle, g.cfg.tunnelWrite)
	if transport == "tcp" {
		relay(r.Context(), client, up,
			func() error { return copyHalf(up, client, fromClient, a) },
			func() error { return copyHalf(client, up, up, a) })
		return
	}
	relay(r.Context(), client, up,
		func() error { return udpFromClient(up, client, fromClient, a) },
		func() error { return udpToClient(client, up, a) })
}

// activity tracks the last data movement in either direction, so a stream
// is idle only when neither side has moved data for the idle timeout.
type activity struct {
	idle, write time.Duration
	last        atomic.Int64
}

func newActivity(idle, write time.Duration) *activity {
	a := &activity{idle: idle, write: write}
	a.touch()
	return a
}

func (a *activity) touch() { a.last.Store(time.Now().UnixNano()) }

func (a *activity) recent() bool { return time.Since(time.Unix(0, a.last.Load())) < a.idle }

// relay runs each half until all have finished. A nil result is a clean
// half-close; any error, or cancellation of ctx, closes both connections.
// Every goroutine has exited before relay returns.
func relay(ctx context.Context, client, up net.Conn, halves ...func() error) {
	closeBoth := func() {
		client.Close()
		up.Close()
	}
	stop := context.AfterFunc(ctx, closeBoth)
	defer stop()
	defer closeBoth()
	errc := make(chan error, len(halves))
	for _, h := range halves {
		go func(h func() error) { errc <- h() }(h)
	}
	for range halves {
		if err := <-errc; err != nil {
			closeBoth()
		}
	}
}

// copyHalf copies a byte stream and propagates EOF as a half-close.
func copyHalf(dst, src net.Conn, r io.Reader, a *activity) error {
	buf := make([]byte, 32<<10)
	for {
		src.SetReadDeadline(time.Now().Add(a.idle))
		n, err := r.Read(buf)
		if n > 0 {
			a.touch()
			dst.SetWriteDeadline(time.Now().Add(a.write))
			if _, werr := dst.Write(buf[:n]); werr != nil {
				return werr
			}
		}
		switch {
		case err == nil:
		case errors.Is(err, io.EOF):
			return closeWrite(dst)
		case isTimeout(err) && a.recent():
		default:
			return err
		}
	}
}

// udpFromClient turns length-prefixed frames into datagrams. Zero, oversized
// or truncated frames fail closed; UDP has no half-close, so a clean EOF
// also ends the tunnel.
func udpFromClient(up, client net.Conn, r io.Reader, a *activity) error {
	var hdr [2]byte
	buf := make([]byte, maxUDPPayload)
	for {
		client.SetReadDeadline(time.Now().Add(a.idle))
		n, err := io.ReadFull(r, hdr[:])
		if err != nil {
			if n == 0 && isTimeout(err) && a.recent() {
				continue
			}
			if n == 0 && errors.Is(err, io.EOF) {
				return io.EOF
			}
			return errFrame
		}
		size := int(binary.BigEndian.Uint16(hdr[:]))
		if size == 0 || size > maxUDPPayload {
			return errFrame
		}
		client.SetReadDeadline(time.Now().Add(a.idle))
		if _, err := io.ReadFull(r, buf[:size]); err != nil {
			return errFrame
		}
		a.touch()
		up.SetWriteDeadline(time.Now().Add(a.write))
		if _, err := up.Write(buf[:size]); err != nil {
			return err
		}
	}
}

// udpToClient frames each upstream datagram, preserving its boundary.
func udpToClient(client, up net.Conn, a *activity) error {
	buf := make([]byte, 2+maxUDPPayload+1)
	for {
		up.SetReadDeadline(time.Now().Add(a.idle))
		n, err := up.Read(buf[2:])
		if err != nil {
			if isTimeout(err) && a.recent() {
				continue
			}
			return err
		}
		if n == 0 || n > maxUDPPayload {
			return errFrame
		}
		a.touch()
		binary.BigEndian.PutUint16(buf[:2], uint16(n))
		client.SetWriteDeadline(time.Now().Add(a.write))
		if _, err := client.Write(buf[:n+2]); err != nil {
			return err
		}
	}
}

func closeWrite(c net.Conn) error {
	if cw, ok := c.(interface{ CloseWrite() error }); ok {
		return cw.CloseWrite()
	}
	return c.Close()
}

func isTimeout(err error) bool {
	var ne net.Error
	return errors.As(err, &ne) && ne.Timeout()
}

// Sunshine PIN pairing.

// parsePair accepts exactly {"pin":"dddd"} with an optional "name" string of
// at most 80 characters and no control characters.
func parsePair(b []byte) (pin, name string, ok bool) {
	top, ok := strictObject(b, 1)
	if !ok {
		return "", "", false
	}
	for k := range top {
		if k != "pin" && k != "name" {
			return "", "", false
		}
	}
	pin, ok = top["pin"].(string)
	if !ok || len(pin) != 4 {
		return "", "", false
	}
	for i := 0; i < len(pin); i++ {
		if pin[i] < '0' || pin[i] > '9' {
			return "", "", false
		}
	}
	if v, present := top["name"]; present {
		if name, ok = v.(string); !ok {
			return "", "", false
		}
	}
	if utf8.RuneCountInString(name) > maxPairName {
		return "", "", false
	}
	for _, c := range name {
		if unicode.IsControl(c) {
			return "", "", false
		}
	}
	if name == "" {
		name = defaultPairName
	}
	return pin, name, true
}

func (g *gateway) servePair(w http.ResponseWriter, r *http.Request) {
	if v := r.Header.Values("X-Kvm-Box-Request"); len(v) != 1 || v[0] != "1" {
		writeError(w, http.StatusForbidden)
		return
	}
	if !jsonContentType(r.Header) {
		writeError(w, http.StatusUnsupportedMediaType)
		return
	}
	body, code := readBody(r, maxPairBody)
	if code != 0 {
		writeError(w, code)
		return
	}
	pin, name, ok := parsePair(body)
	if !ok {
		writeError(w, http.StatusBadRequest)
		return
	}
	release, ok := acquire(g.pairSem)
	if !ok {
		writeError(w, http.StatusTooManyRequests)
		return
	}
	defer release()
	ctx, cancel := context.WithTimeout(r.Context(), g.cfg.pairTimeout)
	defer cancel()
	cred, err := readCredential(g.cfg.credPath, g.cfg.credUID, g.cfg.credGID)
	if err != nil {
		writeError(w, http.StatusServiceUnavailable)
		return
	}
	defer wipe(cred)
	status, code := g.pairSunshine(ctx, cred, pin, name)
	switch {
	case code == http.StatusConflict:
		writeJSON(w, code, []byte(pairCountBody))
	case code != 0:
		writeError(w, code)
	case status:
		writeJSON(w, http.StatusOK, []byte(`{"status":true}`))
	default:
		writeJSON(w, http.StatusOK, []byte(`{"status":false}`))
	}
}

func wipe(b []byte) {
	for i := range b {
		b[i] = 0
	}
}

func ownedExactly(fi fs.FileInfo, mode fs.FileMode, uid, gid uint32) bool {
	st, ok := fi.Sys().(*syscall.Stat_t)
	return ok && fi.Mode() == mode && st.Uid == uid && st.Gid == gid
}

// readCredential reads the generated Sunshine credential from its private
// file: parent uid:gid 0700, file uid:gid 0600, no symlinks. The content must
// be 1–128 bytes of printable non-space ASCII, optionally followed by exactly
// one newline. Nothing else is trimmed or rewritten.
func readCredential(path string, uid, gid uint32) ([]byte, error) {
	if !filepath.IsAbs(path) || filepath.Clean(path) != path {
		return nil, errCredential
	}
	if err := realDir(filepath.Dir(path), uid, gid, 0o700); err != nil {
		return nil, errCredential
	}
	fi, err := os.Lstat(path)
	if err != nil || !ownedExactly(fi, 0o600, uid, gid) || fi.Size() > maxCredential+1 {
		return nil, errCredential
	}
	f, err := os.OpenFile(path, os.O_RDONLY|syscall.O_NOFOLLOW, 0)
	if err != nil {
		return nil, errCredential
	}
	defer f.Close()
	opened, err := f.Stat()
	if err != nil || !os.SameFile(fi, opened) || !ownedExactly(opened, 0o600, uid, gid) {
		return nil, errCredential
	}
	buf := make([]byte, maxCredential+2)
	n, err := io.ReadFull(f, buf)
	if err != io.ErrUnexpectedEOF && err != io.EOF {
		wipe(buf)
		return nil, errCredential
	}
	cred := buf[:n]
	if n > 0 && cred[n-1] == '\n' {
		cred = cred[:n-1]
	}
	valid := len(cred) >= 1 && len(cred) <= maxCredential
	for _, c := range cred {
		if c < 0x21 || c > 0x7e {
			valid = false
		}
	}
	if !valid {
		wipe(buf)
		return nil, errCredential
	}
	return cred, nil
}

type pinRequest struct {
	PairingID string `json:"pairing_id"`
	Pin       string `json:"pin"`
	Name      string `json:"name"`
}

// pairSunshine submits the PIN for the single pending pairing. It returns
// Sunshine's boolean result, or a nonzero status for the client. Sunshine
// bodies, headers and redirects are never passed through.
func (g *gateway) pairSunshine(ctx context.Context, cred []byte, pin, name string) (bool, int) {
	b, code := g.sunshineCall(ctx, http.MethodGet, nil, cred, maxPairList)
	if code != 0 {
		return false, code
	}
	id, count, ok := onePairing(b)
	if !ok {
		return false, http.StatusBadGateway
	}
	if count != 1 {
		return false, http.StatusConflict
	}
	payload, err := json.Marshal(pinRequest{PairingID: id, Pin: pin, Name: name})
	if err != nil {
		return false, http.StatusBadGateway
	}
	b, code = g.sunshineCall(ctx, http.MethodPost, payload, cred, maxPairReply)
	if code != 0 {
		return false, code
	}
	status, ok := pinStatus(b)
	if !ok {
		return false, http.StatusBadGateway
	}
	return status, 0
}

func (g *gateway) sunshineCall(ctx context.Context, method string, body, cred []byte, limit int64) ([]byte, int) {
	var rd io.Reader = http.NoBody
	if body != nil {
		rd = bytes.NewReader(body)
	}
	req, err := http.NewRequestWithContext(ctx, method, sunshinePinURL, rd)
	if err != nil {
		return nil, http.StatusServiceUnavailable
	}
	req.SetBasicAuth(sunshineUser, string(cred))
	req.Header.Set("Accept", "application/json")
	if body != nil {
		req.Header.Set("Content-Type", "application/json")
	}
	resp, err := g.sunshine.Do(req)
	if err != nil {
		return nil, http.StatusServiceUnavailable
	}
	defer resp.Body.Close()
	switch resp.StatusCode {
	case http.StatusOK:
	case http.StatusUnauthorized, http.StatusForbidden:
		// Credential mismatch is an integration fault, not a client error.
		return nil, http.StatusServiceUnavailable
	default:
		return nil, http.StatusBadGateway
	}
	b, err := io.ReadAll(io.LimitReader(resp.Body, limit+1))
	if err != nil || int64(len(b)) > limit {
		return nil, http.StatusBadGateway
	}
	return b, 0
}

// onePairing counts pending pairings and returns the ID when there is one.
func onePairing(b []byte) (id string, count int, ok bool) {
	var list struct {
		Pairings *[]struct {
			ID *string `json:"id"`
		} `json:"pairings"`
	}
	if !decodeOne(b, &list) || list.Pairings == nil {
		return "", 0, false
	}
	ps := *list.Pairings
	if len(ps) != 1 {
		return "", len(ps), true
	}
	p := ps[0].ID
	if p == nil || !validPairingID(*p) {
		return "", 0, false
	}
	return *p, 1, true
}

func validPairingID(s string) bool {
	if len(s) < 1 || len(s) > maxPairingID {
		return false
	}
	for i := 0; i < len(s); i++ {
		if s[i] < 0x21 || s[i] > 0x7e {
			return false
		}
	}
	return true
}

// pinStatus accepts Sunshine's status as a JSON boolean or as the strings
// "true"/"false" (older property-tree encoders emit strings).
func pinStatus(b []byte) (bool, bool) {
	var reply struct {
		Status json.RawMessage `json:"status"`
	}
	if !decodeOne(b, &reply) {
		return false, false
	}
	switch string(reply.Status) {
	case "true", `"true"`:
		return true, true
	case "false", `"false"`:
		return false, true
	}
	return false, false
}
