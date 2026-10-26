//go:build linux

// Native companion control gateway for the measured desktop profile.
//
// This unprivileged process (UID 1002) listens on 127.0.0.1:19080. The Wasm
// controller authenticates the owner and is the only intended client. TCP
// carries no peer identity, so nothing here authenticates the owner: the
// measured supervisor's nftables policy must restrict TCP 19080 to the Wasm
// runtime UID before any workload starts. See notes.md.
package main

import (
	"bufio"
	"bytes"
	"context"
	"crypto/sha1"
	"encoding/base64"
	"encoding/binary"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"log"
	"net"
	"net/http"
	"net/http/httputil"
	"net/url"
	"os"
	"os/signal"
	"path/filepath"
	"strconv"
	"strings"
	"sync"
	"sync/atomic"
	"syscall"
	"time"
)

// Production endpoints and limits. None of these are configurable at runtime.
const (
	listenAddr    = "127.0.0.1:19080"
	innerAddr     = "127.0.0.1:18080"
	controlSocket = "/run/native-desktop/control.sock"

	gatewayUID = 1002
	gatewayGID = 1002
	rootUID    = 0

	maxActive      = 32
	maxBodyBytes   = 8 << 20
	maxHeaderBytes = 16 << 10
	maxCtlReply    = 4096

	ctlConnectTimeout   = 3 * time.Second
	ctlIOTimeout        = 5 * time.Second
	upstreamDialTimeout = 3 * time.Second
	handshakeTimeout    = 10 * time.Second
	tunnelIdleTimeout   = 300 * time.Second
	tunnelWriteTimeout  = 30 * time.Second

	tunnelProtocol = "kvm-box-tunnel"
	wsGUID         = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11"
)

var errControl = errors.New("control unavailable")

// config exists so tests can substitute ephemeral ports, temporary socket
// directories and their own UID. Production always uses productionConfig.
type config struct {
	innerAddr    string
	controlPath  string
	ctlDirUID    uint32
	ctlSockUID   uint32
	ctlGID       uint32
	ctlPeerUID   uint32
	ctlConnect   time.Duration
	ctlIO        time.Duration
	upstreamDial time.Duration
	tunnelIdle   time.Duration
	maxActive    int
}

func productionConfig() config {
	return config{
		innerAddr:    innerAddr,
		controlPath:  controlSocket,
		ctlDirUID:    rootUID,
		ctlSockUID:   rootUID,
		ctlGID:       gatewayGID,
		ctlPeerUID:   rootUID,
		ctlConnect:   ctlConnectTimeout,
		ctlIO:        ctlIOTimeout,
		upstreamDial: upstreamDialTimeout,
		tunnelIdle:   tunnelIdleTimeout,
		maxActive:    maxActive,
	}
}

func main() {
	if err := run(); err != nil {
		fmt.Fprintln(os.Stderr, "native companion gateway:", err)
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
// credential variables. The supervisor must supply a clean environment.
func startupCheck(uid, euid int, lookup func(string) (string, bool), args []string) error {
	if len(args) > 1 {
		return errors.New("arguments are not accepted")
	}
	if uid != gatewayUID || euid != gatewayUID {
		return errors.New("must run as uid 1002")
	}
	for _, k := range []string{"KVMBOX_OWNER_PASSWORD", "KVMBOX_ACCESS_SHA256", "KVMBOX_AUTOMATION_KEY"} {
		if _, ok := lookup(k); ok {
			return errors.New("inherited owner credential variable refused")
		}
	}
	return nil
}

type gateway struct {
	cfg    config
	ctx    context.Context
	cancel context.CancelFunc
	slots  chan struct{}
	ctlSem chan struct{}
	proxy  *httputil.ReverseProxy
	srv    *http.Server
}

func newGateway(cfg config) *gateway {
	ctx, cancel := context.WithCancel(context.Background())
	g := &gateway{
		cfg:    cfg,
		ctx:    ctx,
		cancel: cancel,
		slots:  make(chan struct{}, cfg.maxActive),
		ctlSem: make(chan struct{}, 1),
	}
	discard := log.New(io.Discard, "", 0)
	dialer := &net.Dialer{Timeout: cfg.upstreamDial}
	g.proxy = &httputil.ReverseProxy{
		Rewrite: g.rewrite,
		Transport: &http.Transport{
			Proxy: nil,
			// The destination is fixed regardless of the request.
			DialContext: func(ctx context.Context, _, _ string) (net.Conn, error) {
				return dialer.DialContext(ctx, "tcp", cfg.innerAddr)
			},
			MaxConnsPerHost:        cfg.maxActive,
			MaxIdleConnsPerHost:    8,
			IdleConnTimeout:        60 * time.Second,
			ResponseHeaderTimeout:  130 * time.Second,
			MaxResponseHeaderBytes: 64 << 10,
			DisableCompression:     true,
		},
		FlushInterval:  -1,
		ErrorLog:       discard,
		ModifyResponse: modifyResponse,
		ErrorHandler: func(w http.ResponseWriter, _ *http.Request, _ error) {
			writeError(w, http.StatusBadGateway)
		},
	}
	g.srv = &http.Server{
		Handler:                      g,
		ReadHeaderTimeout:            5 * time.Second,
		ReadTimeout:                  60 * time.Second,
		WriteTimeout:                 10 * time.Minute,
		IdleTimeout:                  60 * time.Second,
		MaxHeaderBytes:               maxHeaderBytes,
		DisableGeneralOptionsHandler: true,
		BaseContext:                  func(net.Listener) context.Context { return ctx },
		ErrorLog:                     discard,
	}
	return g
}

// Serve accepts at most cfg.maxActive connections at a time, including
// upgraded streams, which keep their slot until both sides are closed.
func (g *gateway) Serve(ln net.Listener) error {
	return g.srv.Serve(&limitListener{Listener: ln, slots: g.slots})
}

// Close stops the server and tears down every hijacked stream.
func (g *gateway) Close() error {
	g.cancel()
	return g.srv.Close()
}

var busyResponse = func() string {
	body := `{"error":"gateway busy"}` + "\n"
	return "HTTP/1.1 503 Service Unavailable\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: " +
		strconv.Itoa(len(body)) + "\r\n\r\n" + body
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

// Routes proxied to the inner desktop gateway. Everything else is refused
// here without contacting it. /app/, /core/ and /vendor/ are noVNC's static
// asset directories loaded by /vnc.html.
type route struct {
	path    string
	prefix  bool
	methods []string
	query   bool
	upgrade string
}

var (
	getOnly     = []string{http.MethodGet}
	getHead     = []string{http.MethodGet, http.MethodHead}
	getHeadPost = []string{http.MethodGet, http.MethodHead, http.MethodPost}
)

var routes = []route{
	{path: "/exec", methods: []string{http.MethodPost}},
	{path: "/computer", methods: []string{http.MethodPost}},
	{path: "/vnc.html", methods: getHead, query: true},
	{path: "/app/", prefix: true, methods: getHead, query: true},
	{path: "/core/", prefix: true, methods: getHead, query: true},
	{path: "/vendor/", prefix: true, methods: getHead, query: true},
	{path: "/compute/", prefix: true, methods: getHeadPost, query: true},
	{path: "/api/moonlight/pair", methods: []string{http.MethodPost}},
	{path: "/websockify", methods: getOnly, upgrade: "websocket"},
	{path: "/api/moonlight/tunnel", methods: getOnly, upgrade: tunnelProtocol},
}

func findRoute(p string) *route {
	for i := range routes {
		rt := &routes[i]
		if p == rt.path || rt.prefix && strings.HasPrefix(p, rt.path) {
			return rt
		}
	}
	return nil
}

func (rt *route) allows(method string) bool {
	for _, m := range rt.methods {
		if m == method {
			return true
		}
	}
	return false
}

// Fixed Sunshine destinations from the desktop contract, in canonical query form.
var tunnelPorts = map[string][]string{
	"tcp": {"47984", "47989", "48010"},
	"udp": {"47998", "47999", "48000"},
}

// Request headers forwarded upstream; all others (Authorization, Cookie,
// Forwarded, X-Forwarded-*, X-Real-IP, internal auth headers...) are dropped.
var httpForward = []string{
	"Accept", "Accept-Encoding", "Accept-Language", "Cache-Control", "Content-Type",
	"If-Match", "If-Modified-Since", "If-None-Match", "If-Range", "Pragma", "Range",
	"User-Agent", "X-Compute-Authorization", "X-Kvm-Box-Request",
}

var wsForward = []string{
	"Sec-Websocket-Key", "Sec-Websocket-Version", "Sec-Websocket-Protocol",
	"Sec-Websocket-Extensions", "User-Agent",
}

var singletonHeaders = []string{
	"Host", "Authorization", "Proxy-Authorization", "Content-Length", "Content-Type",
	"Upgrade", "Origin", "Connection", "X-Compute-Authorization", "X-Kvm-Box-Request",
	"Sec-Websocket-Key", "Sec-Websocket-Version",
}

func (g *gateway) ServeHTTP(w http.ResponseWriter, r *http.Request) {
	path, query, hasQuery, code := admit(r)
	if code != 0 {
		writeError(w, code)
		return
	}
	lower := strings.ToLower(path)
	if lower == "/v1" || strings.HasPrefix(lower, "/v1/") {
		g.serveControl(w, r, path, hasQuery)
		return
	}
	rt := findRoute(path)
	if rt == nil {
		writeError(w, http.StatusNotFound)
		return
	}
	if !rt.allows(r.Method) {
		writeError(w, http.StatusMethodNotAllowed)
		return
	}
	if rt.upgrade != "" {
		g.serveUpgrade(w, r, rt, query, hasQuery)
		return
	}
	if len(r.Header.Values("Upgrade")) > 0 || connectionHasUpgrade(r.Header) {
		writeError(w, http.StatusBadRequest)
		return
	}
	if hasQuery && (!rt.query || !validQuery(query)) {
		writeError(w, http.StatusBadRequest)
		return
	}
	r.Body = http.MaxBytesReader(w, r.Body, maxBodyBytes)
	g.proxy.ServeHTTP(w, r)
}

// admit applies the checks common to every route. It returns the raw path
// and query, or a nonzero HTTP status.
func admit(r *http.Request) (path, query string, hasQuery bool, code int) {
	switch r.Method {
	case http.MethodGet, http.MethodHead, http.MethodPost:
	default:
		return "", "", false, http.StatusMethodNotAllowed
	}
	if r.ProtoMajor != 1 {
		return "", "", false, http.StatusHTTPVersionNotSupported
	}
	if len(r.TransferEncoding) > 0 || len(r.Header.Values("Transfer-Encoding")) > 0 {
		return "", "", false, http.StatusBadRequest
	}
	if requestHeaderBytes(r) > maxHeaderBytes {
		return "", "", false, http.StatusRequestHeaderFieldsTooLarge
	}
	for _, k := range singletonHeaders {
		if len(r.Header.Values(k)) > 1 {
			return "", "", false, http.StatusBadRequest
		}
	}
	if !validHost(r.Host) {
		return "", "", false, http.StatusBadRequest
	}
	if !originAllowed(r) {
		return "", "", false, http.StatusForbidden
	}
	if r.ContentLength < 0 {
		return "", "", false, http.StatusBadRequest
	}
	if r.ContentLength > maxBodyBytes {
		return "", "", false, http.StatusRequestEntityTooLarge
	}
	// Origin-form only: rejects absolute URLs, authority form and "*".
	path, query, hasQuery = strings.Cut(r.RequestURI, "?")
	if !validPath(path) || path != r.URL.Path {
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

// Origin may be absent (the Wasm controller strips it). If present it must
// name exactly the requested host.
func originAllowed(r *http.Request) bool {
	vs := r.Header.Values("Origin")
	if len(vs) == 0 {
		return true
	}
	for _, scheme := range []string{"http://", "https://"} {
		if rest, ok := strings.CutPrefix(vs[0], scheme); ok && strings.EqualFold(rest, r.Host) {
			return true
		}
	}
	return false
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

func validHost(h string) bool {
	if h == "" || len(h) > 255 {
		return false
	}
	for i := 0; i < len(h); i++ {
		c := h[i]
		if !isAlnum(c) && c != '.' && c != '-' && c != ':' && c != '[' && c != ']' {
			return false
		}
	}
	return true
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

func safeHeaderValue(v string) bool {
	if len(v) > 1024 {
		return false
	}
	for i := 0; i < len(v); i++ {
		if (v[i] < 0x20 || v[i] > 0x7e) && v[i] != '\t' {
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

func upgradeTokenOK(proto string, vs []string) bool {
	if len(vs) != 1 {
		return false
	}
	if proto == "websocket" {
		return strings.EqualFold(vs[0], proto)
	}
	return vs[0] == proto
}

func validWebSocketRequest(h http.Header) bool {
	if v := h.Values("Sec-Websocket-Version"); len(v) != 1 || v[0] != "13" {
		return false
	}
	k := h.Values("Sec-Websocket-Key")
	if len(k) != 1 {
		return false
	}
	raw, err := base64.StdEncoding.DecodeString(k[0])
	if err != nil || len(raw) != 16 {
		return false
	}
	for _, name := range []string{"Sec-Websocket-Protocol", "Sec-Websocket-Extensions"} {
		if v := h.Values(name); len(v) > 1 || len(v) == 1 && !safeHeaderValue(v[0]) {
			return false
		}
	}
	return true
}

func wsAccept(key string) string {
	sum := sha1.Sum([]byte(key + wsGUID))
	return base64.StdEncoding.EncodeToString(sum[:])
}

func offered(list, proto string) bool {
	for _, p := range strings.Split(list, ",") {
		if strings.TrimSpace(p) == proto {
			return true
		}
	}
	return false
}

// canonicalTunnelQuery accepts exactly one port and one transport naming a
// fixed Sunshine destination and returns the canonical "port=N&transport=T".
func canonicalTunnelQuery(raw string) (string, bool) {
	parts := strings.Split(raw, "&")
	if len(parts) != 2 {
		return "", false
	}
	var port, transport string
	for _, p := range parts {
		k, v, ok := strings.Cut(p, "=")
		if !ok || v == "" {
			return "", false
		}
		switch {
		case k == "port" && port == "":
			port = v
		case k == "transport" && transport == "":
			transport = v
		default:
			return "", false
		}
	}
	for _, p := range tunnelPorts[transport] {
		if p == port {
			return "port=" + port + "&transport=" + transport, true
		}
	}
	return "", false
}

func allowHeaders(in http.Header, names []string) http.Header {
	out := http.Header{}
	for _, n := range names {
		if vs := in.Values(n); len(vs) > 0 {
			out[http.CanonicalHeaderKey(n)] = append([]string(nil), vs...)
		}
	}
	return out
}

func (g *gateway) rewrite(pr *httputil.ProxyRequest) {
	pr.Out.URL = &url.URL{Scheme: "http", Host: g.cfg.innerAddr, Path: pr.In.URL.Path, RawQuery: pr.In.URL.RawQuery}
	pr.Out.Host = g.cfg.innerAddr
	pr.Out.Header = allowHeaders(pr.In.Header, httpForward)
}

// modifyResponse never lets an upstream redirect leave the proxied origin and
// refuses unexpected protocol switches on plain routes.
func modifyResponse(resp *http.Response) error {
	if resp.StatusCode == http.StatusSwitchingProtocols {
		return errors.New("unexpected upgrade")
	}
	resp.Header.Del("Set-Cookie")
	if loc := resp.Header.Values("Location"); len(loc) > 0 {
		if len(loc) != 1 || !strings.HasPrefix(loc[0], "/") || strings.HasPrefix(loc[0], "//") || strings.Contains(loc[0], "\\") {
			return errors.New("redirect refused")
		}
	}
	return nil
}

func writeError(w http.ResponseWriter, code int) {
	h := w.Header()
	h.Set("Content-Type", "application/json")
	h.Set("Cache-Control", "no-store")
	h.Set("X-Content-Type-Options", "nosniff")
	w.WriteHeader(code)
	fmt.Fprintf(w, "{\"error\":%q}\n", strings.ToLower(http.StatusText(code)))
}

// Control routes.

type ctlStatus struct {
	Status    string `json:"status"`
	Pid       int64  `json:"pid"`
	Vcpus     int64  `json:"vcpus"`
	MemoryMiB int64  `json:"memoryMiB"`
}

func (g *gateway) serveControl(w http.ResponseWriter, r *http.Request, path string, hasQuery bool) {
	var cmd byte
	var method string
	switch path {
	case "/v1/status":
		cmd, method = 'Q', http.MethodGet
	case "/v1/start":
		cmd, method = 'B', http.MethodPost
	case "/v1/stop":
		cmd, method = 'H', http.MethodPost
	default:
		writeError(w, http.StatusNotFound)
		return
	}
	if r.Method != method {
		writeError(w, http.StatusMethodNotAllowed)
		return
	}
	if hasQuery || r.ContentLength != 0 || len(r.Header.Values("Upgrade")) > 0 {
		writeError(w, http.StatusBadRequest)
		return
	}
	if method == http.MethodPost {
		if v := r.Header.Values("X-Kvm-Box-Request"); len(v) != 1 || v[0] != "1" {
			writeError(w, http.StatusForbidden)
			return
		}
	}
	st, err := g.control(r.Context(), cmd)
	if err != nil {
		writeError(w, http.StatusServiceUnavailable)
		return
	}
	body, err := json.Marshal(st)
	if err != nil {
		writeError(w, http.StatusServiceUnavailable)
		return
	}
	h := w.Header()
	h.Set("Content-Type", "application/json")
	h.Set("Cache-Control", "no-store")
	h.Set("X-Content-Type-Options", "nosniff")
	w.WriteHeader(http.StatusOK)
	w.Write(body)
}

// control performs one exchange with the root supervisor. The socket path is
// checked before connecting and the server's SO_PEERCRED UID before the
// command byte is written; there is no fallback.
func (g *gateway) control(ctx context.Context, cmd byte) (ctlStatus, error) {
	if ctx.Err() != nil {
		return ctlStatus{}, errControl
	}
	select {
	case g.ctlSem <- struct{}{}:
		defer func() { <-g.ctlSem }()
	case <-ctx.Done():
		return ctlStatus{}, errControl
	}
	if ctx.Err() != nil {
		return ctlStatus{}, errControl
	}
	if err := checkControlPath(g.cfg.controlPath, g.cfg.ctlDirUID, g.cfg.ctlSockUID, g.cfg.ctlGID); err != nil {
		return ctlStatus{}, errControl
	}
	d := net.Dialer{Timeout: g.cfg.ctlConnect}
	c, err := d.DialContext(ctx, "unix", g.cfg.controlPath)
	if err != nil {
		return ctlStatus{}, errControl
	}
	defer c.Close()
	uc, ok := c.(*net.UnixConn)
	if !ok {
		return ctlStatus{}, errControl
	}
	// Install one exchange deadline before the cancellation callback. Resetting
	// a read/write deadline later could overwrite cancellation's expired one.
	uc.SetDeadline(time.Now().Add(g.cfg.ctlIO))
	stop := context.AfterFunc(ctx, func() { uc.SetDeadline(time.Now()) })
	defer stop()
	if uid, err := unixPeerUID(uc); err != nil || uid != g.cfg.ctlPeerUID {
		return ctlStatus{}, errControl
	}
	if _, err := uc.Write([]byte{cmd}); err != nil {
		return ctlStatus{}, errControl
	}
	if err := uc.CloseWrite(); err != nil {
		return ctlStatus{}, errControl
	}
	var hdr [4]byte
	if _, err := io.ReadFull(uc, hdr[:]); err != nil {
		return ctlStatus{}, errControl
	}
	n := binary.BigEndian.Uint32(hdr[:])
	if n < 1 || n > maxCtlReply {
		return ctlStatus{}, errControl
	}
	body := make([]byte, n)
	if _, err := io.ReadFull(uc, body); err != nil {
		return ctlStatus{}, errControl
	}
	var extra [1]byte
	if _, err := io.ReadFull(uc, extra[:]); err != io.EOF {
		return ctlStatus{}, errControl
	}
	if ctx.Err() != nil {
		return ctlStatus{}, errControl
	}
	return parseStatus(body)
}

// checkControlPath requires a root-owned parent directory (0750, group 1002)
// and a root-owned socket (0660, group 1002). Lstat refuses symlinks.
func checkControlPath(path string, dirUID, sockUID, gid uint32) error {
	if !filepath.IsAbs(path) {
		return errControl
	}
	di, err := os.Lstat(filepath.Dir(path))
	if err != nil {
		return errControl
	}
	ds, ok := di.Sys().(*syscall.Stat_t)
	if !ok || di.Mode() != os.ModeDir|0o750 || ds.Uid != dirUID || ds.Gid != gid {
		return errControl
	}
	si, err := os.Lstat(path)
	if err != nil {
		return errControl
	}
	ss, ok := si.Sys().(*syscall.Stat_t)
	if !ok || si.Mode() != os.ModeSocket|0o660 || ss.Uid != sockUID || ss.Gid != gid {
		return errControl
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

// parseStatus accepts only the canonical encoding: exactly the bytes that
// json.Marshal produces for ctlStatus. That refuses whitespace, reordered,
// duplicate, missing, case-folded or unknown keys, and non-integer numbers.
func parseStatus(b []byte) (ctlStatus, error) {
	var st ctlStatus
	dec := json.NewDecoder(bytes.NewReader(b))
	dec.DisallowUnknownFields()
	if err := dec.Decode(&st); err != nil {
		return ctlStatus{}, errControl
	}
	canon, err := json.Marshal(st)
	if err != nil || !bytes.Equal(canon, b) {
		return ctlStatus{}, errControl
	}
	switch st.Status {
	case "stopped", "starting", "running", "stopping", "failed":
	default:
		return ctlStatus{}, errControl
	}
	if st.Pid < 0 || st.Pid > 1<<31-1 || st.Vcpus < 1 || st.Vcpus > 16 || st.MemoryMiB < 8192 || st.MemoryMiB > 65536 {
		return ctlStatus{}, errControl
	}
	return st, nil
}

// Upgraded streams (noVNC WebSocket and the Moonlight tunnel).

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

func (g *gateway) serveUpgrade(w http.ResponseWriter, r *http.Request, rt *route, query string, hasQuery bool) {
	if r.ContentLength != 0 || !connectionIsUpgrade(r.Header) || !upgradeTokenOK(rt.upgrade, r.Header.Values("Upgrade")) {
		writeError(w, http.StatusBadRequest)
		return
	}
	target := rt.path
	var fwd []string
	switch rt.upgrade {
	case "websocket":
		if hasQuery || !validWebSocketRequest(r.Header) {
			writeError(w, http.StatusBadRequest)
			return
		}
		fwd = wsForward
	case tunnelProtocol:
		q, ok := canonicalTunnelQuery(query)
		if !hasQuery || !ok {
			writeError(w, http.StatusBadRequest)
			return
		}
		target += "?" + q
	}
	var req bytes.Buffer
	fmt.Fprintf(&req, "GET %s HTTP/1.1\r\nHost: %s\r\nConnection: Upgrade\r\nUpgrade: %s\r\n", target, g.cfg.innerAddr, rt.upgrade)
	for _, k := range fwd {
		if v := r.Header.Get(k); v != "" {
			if !safeHeaderValue(v) {
				writeError(w, http.StatusBadRequest)
				return
			}
			fmt.Fprintf(&req, "%s: %s\r\n", k, v)
		}
	}
	req.WriteString("\r\n")

	d := net.Dialer{Timeout: g.cfg.upstreamDial}
	up, err := d.DialContext(r.Context(), "tcp", g.cfg.innerAddr)
	if err != nil {
		writeError(w, http.StatusBadGateway)
		return
	}
	handedOff := false
	defer func() {
		if !handedOff {
			up.Close()
		}
	}()
	up.SetDeadline(time.Now().Add(handshakeTimeout))
	if _, err := up.Write(req.Bytes()); err != nil {
		writeError(w, http.StatusBadGateway)
		return
	}
	ubr := bufio.NewReaderSize(&handshakeReader{r: up, n: maxHeaderBytes}, 4096)
	resp, err := http.ReadResponse(ubr, &http.Request{Method: http.MethodGet})
	if err != nil {
		writeError(w, http.StatusBadGateway)
		return
	}
	if resp.StatusCode != http.StatusSwitchingProtocols {
		code := http.StatusBadGateway
		if resp.StatusCode >= 400 && resp.StatusCode <= 599 {
			code = resp.StatusCode
		}
		writeError(w, code)
		return
	}
	if !connectionIsUpgrade(resp.Header) || !upgradeTokenOK(rt.upgrade, resp.Header.Values("Upgrade")) {
		writeError(w, http.StatusBadGateway)
		return
	}
	var extra []string
	if rt.upgrade == "websocket" {
		acc := resp.Header.Values("Sec-Websocket-Accept")
		if len(acc) != 1 || acc[0] != wsAccept(r.Header.Get("Sec-Websocket-Key")) {
			writeError(w, http.StatusBadGateway)
			return
		}
		extra = append(extra, "Sec-WebSocket-Accept: "+acc[0])
		if p := resp.Header.Values("Sec-Websocket-Protocol"); len(p) > 0 {
			if len(p) != 1 || !safeHeaderValue(p[0]) || !offered(r.Header.Get("Sec-Websocket-Protocol"), p[0]) {
				writeError(w, http.StatusBadGateway)
				return
			}
			extra = append(extra, "Sec-WebSocket-Protocol: "+p[0])
		}
		if e := resp.Header.Values("Sec-Websocket-Extensions"); len(e) > 0 {
			if len(e) != 1 || !safeHeaderValue(e[0]) || r.Header.Get("Sec-Websocket-Extensions") == "" {
				writeError(w, http.StatusBadGateway)
				return
			}
			extra = append(extra, "Sec-WebSocket-Extensions: "+e[0])
		}
	}

	hj, ok := w.(http.Hijacker)
	if !ok {
		writeError(w, http.StatusInternalServerError)
		return
	}
	client, cbuf, err := hj.Hijack()
	if err != nil {
		return
	}
	handedOff = true
	client.SetDeadline(time.Time{})
	up.SetDeadline(time.Time{})
	var out bytes.Buffer
	out.WriteString("HTTP/1.1 101 Switching Protocols\r\nUpgrade: " + rt.upgrade + "\r\nConnection: Upgrade\r\n")
	for _, h := range extra {
		out.WriteString(h + "\r\n")
	}
	out.WriteString("\r\n")
	client.SetWriteDeadline(time.Now().Add(tunnelWriteTimeout))
	if _, err := client.Write(out.Bytes()); err != nil {
		client.Close()
		up.Close()
		return
	}
	g.pipe(r.Context(), client, up, drain(cbuf.Reader, client), drain(ubr, up))
}

// pipe copies both directions until both have finished. EOF in one direction
// is propagated as a half-close; any error, idle expiry or gateway shutdown
// closes both connections. Both goroutines always exit before pipe returns.
func (g *gateway) pipe(ctx context.Context, client, up net.Conn, fromClient, fromUp io.Reader) {
	var last atomic.Int64
	last.Store(time.Now().UnixNano())
	closeBoth := func() {
		client.Close()
		up.Close()
	}
	stop := context.AfterFunc(ctx, closeBoth)
	defer stop()
	defer closeBoth()
	errc := make(chan error, 2)
	go func() { errc <- g.copyHalf(up, client, fromClient, &last) }()
	go func() { errc <- g.copyHalf(client, up, fromUp, &last) }()
	for i := 0; i < 2; i++ {
		if err := <-errc; err != nil {
			closeBoth()
		}
	}
}

// copyHalf treats the stream as idle only when neither direction has moved
// data for the idle timeout.
func (g *gateway) copyHalf(dst, src net.Conn, r io.Reader, last *atomic.Int64) error {
	buf := make([]byte, 32<<10)
	for {
		src.SetReadDeadline(time.Now().Add(g.cfg.tunnelIdle))
		n, err := r.Read(buf)
		if n > 0 {
			last.Store(time.Now().UnixNano())
			dst.SetWriteDeadline(time.Now().Add(tunnelWriteTimeout))
			if _, werr := dst.Write(buf[:n]); werr != nil {
				return werr
			}
		}
		switch {
		case err == nil:
		case errors.Is(err, io.EOF):
			return closeWrite(dst)
		case isTimeout(err) && time.Since(time.Unix(0, last.Load())) < g.cfg.tunnelIdle:
		default:
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
