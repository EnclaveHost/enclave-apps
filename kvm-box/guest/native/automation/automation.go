//go:build linux

// Desktop automation runs with the same unprivileged identity as the desktop.
// Only the measured inner gateway may connect. Owner/API-key authentication
// belongs in the Wasm controller; no credential is accepted or loaded here.
package main

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net"
	"net/http"
	"os"
	"os/exec"
	"os/signal"
	"path/filepath"
	"strconv"
	"strings"
	"sync"
	"syscall"
	"time"
)

const (
	socketDir      = "/run/native-automation"
	socketPath     = socketDir + "/desktop.sock"
	desktopUID     = 1000
	clientUID      = 1003
	maxRequest     = 64 << 10
	maxOutput      = 1 << 20
	maxConnections = 4
)

// Deliberately constructed, not copied from the daemon's environment. In
// particular commands never inherit a released owner key, proxy credential,
// arbitrary loader override, or service configuration.
var desktopEnvironment = []string{
	"PATH=/usr/local/bin:/usr/bin:/bin", "HOME=/home/desktop", "LANG=C.UTF-8",
	"DISPLAY=:0", "XDG_RUNTIME_DIR=/run/user/1000", "XAUTHORITY=/run/user/1000/Xauthority",
	"DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus", "PULSE_SERVER=unix:/run/user/1000/pulse/native",
	"VK_DRIVER_FILES=/opt/shield/mesa/share/vulkan/icd.d/lvp_icd.x86_64.json",
	"VK_ICD_FILENAMES=/opt/shield/mesa/share/vulkan/icd.d/lvp_icd.x86_64.json",
	"LIBGL_DRIVERS_PATH=/opt/shield/mesa/lib/dri", "LD_LIBRARY_PATH=/opt/shield/mesa/lib",
	"LIBGL_ALWAYS_SOFTWARE=1", "GALLIUM_DRIVER=zink", "MESA_LOADER_DRIVER_OVERRIDE=zink",
	"LIBGL_KOPPER_DRI2=true", "ZINK_DESCRIPTORS=lazy", "LP_NUM_THREADS=2",
}

type execRequest struct {
	Cmd      string `json:"cmd"`
	Timeout  *int   `json:"timeout_s,omitempty"`
	MaxBytes *int   `json:"max_bytes,omitempty"`
}

type execResult struct {
	OK        bool   `json:"ok"`
	ExitCode  *int   `json:"exitCode,omitempty"`
	Output    string `json:"output"`
	Truncated bool   `json:"truncated"`
	MS        int64  `json:"ms"`
	Error     string `json:"error,omitempty"`
}

// Output continues to drain after the cap, so a noisy program cannot block
// forever on its pipe or grow the service's heap without bound.
type boundedOutput struct {
	mu        sync.Mutex
	data      []byte
	limit     int
	truncated bool
}

func (b *boundedOutput) Write(p []byte) (int, error) {
	b.mu.Lock()
	defer b.mu.Unlock()
	n := len(p)
	remaining := b.limit - len(b.data)
	if len(p) > remaining {
		p = p[:remaining]
		b.truncated = true
	}
	b.data = append(b.data, p...)
	return n, nil
}

func clamp(n, low, high int) int {
	if n < low {
		return low
	}
	if n > high {
		return high
	}
	return n
}

func execute(ctx context.Context, r execRequest, dir string, env []string) execResult {
	start := time.Now()
	seconds, cap := 30, 64<<10
	if r.Timeout != nil {
		seconds = clamp(*r.Timeout, 1, 120)
	}
	if r.MaxBytes != nil {
		cap = clamp(*r.MaxBytes, 1024, maxOutput)
	}
	ctx, cancel := context.WithTimeout(ctx, time.Duration(seconds)*time.Second)
	defer cancel()
	out := &boundedOutput{limit: cap}
	cmd := exec.CommandContext(ctx, "/bin/sh", "-c", r.Cmd)
	cmd.Dir = dir
	cmd.Env = append([]string(nil), env...)
	cmd.Stdin = nil
	cmd.Stdout = out
	cmd.Stderr = out
	cmd.SysProcAttr = &syscall.SysProcAttr{Setpgid: true}
	// Kill the command's process group on cancellation, not just the shell.
	// Reaping and a finite pipe wait also bound a child which keeps stdout open.
	cmd.Cancel = func() error {
		if cmd.Process == nil {
			return os.ErrProcessDone
		}
		err := syscall.Kill(-cmd.Process.Pid, syscall.SIGKILL)
		if errors.Is(err, syscall.ESRCH) {
			return os.ErrProcessDone
		}
		return err
	}
	cmd.WaitDelay = 250 * time.Millisecond
	err := cmd.Run()
	if errors.Is(err, exec.ErrWaitDelay) && cmd.Process != nil {
		_ = syscall.Kill(-cmd.Process.Pid, syscall.SIGKILL)
	}
	out.mu.Lock()
	result := execResult{Output: strings.ToValidUTF8(string(out.data), "�"), Truncated: out.truncated, MS: time.Since(start).Milliseconds()}
	out.mu.Unlock()
	switch {
	case ctx.Err() != nil:
		result.Error = "command cancelled or timed out"
	case errors.Is(err, exec.ErrWaitDelay):
		result.Error = "command descendants kept the output pipe open"
	case err == nil:
		code := 0
		result.OK = true
		result.ExitCode = &code
	default:
		var exit *exec.ExitError
		if errors.As(err, &exit) && exit.ExitCode() >= 0 {
			code := exit.ExitCode()
			result.OK = true
			result.ExitCode = &code
		} else {
			result.Error = "command could not complete"
		}
	}
	return result
}

type service struct {
	dir           string
	env           []string
	slots         chan struct{}
	computerSlots chan struct{}
	grab          func(context.Context) (screen, error)
	send          func(context.Context, []string) error
}

func jsonReply(w http.ResponseWriter, code int, value any) {
	body, err := json.Marshal(value)
	if err != nil {
		code = 500
		body = []byte(`{"ok":false,"error":"response encoding failed"}`)
	}
	w.Header().Set("Content-Length", strconv.Itoa(len(body)))
	w.Header().Set("Content-Type", "application/json")
	w.Header().Set("X-Content-Type-Options", "nosniff")
	w.Header().Set("Cache-Control", "no-store")
	w.Header().Set("Content-Security-Policy", "default-src 'none'; sandbox")
	w.WriteHeader(code)
	_, _ = w.Write(body)
}

func (s *service) ServeHTTP(w http.ResponseWriter, r *http.Request) {
	rc := http.NewResponseController(w)
	_ = rc.SetReadDeadline(time.Now().Add(5 * time.Second))
	_ = rc.SetWriteDeadline(time.Now().Add(125 * time.Second))
	if r.Method != "POST" || (r.URL.Path != "/exec" && r.URL.Path != "/computer") || r.URL.RawQuery != "" || r.URL.RawPath != "" {
		jsonReply(w, 404, map[string]any{"ok": false, "error": "endpoint refused"})
		return
	}
	select {
	case s.slots <- struct{}{}:
		defer func() { <-s.slots }()
	default:
		jsonReply(w, 429, map[string]any{"ok": false, "error": "desktop busy"})
		return
	}
	r.Body = http.MaxBytesReader(w, r.Body, maxRequest)
	defer r.Body.Close()
	if r.URL.Path == "/computer" {
		select {
		case s.computerSlots <- struct{}{}:
			defer func() { <-s.computerSlots }()
		default:
			jsonReply(w, 429, map[string]any{"ok": false, "error": "computer busy"})
			return
		}
		body, e := decodeComputer(r.Body)
		if e != nil {
			jsonReply(w, 400, map[string]any{"ok": false, "error": "invalid computer request"})
			return
		}
		grab, send := s.grab, s.send
		if grab == nil {
			grab = capture
		}
		if send == nil {
			send = input
		}
		value, e := computer(r.Context(), body, grab, send)
		if e != nil {
			jsonReply(w, 400, map[string]any{"ok": false, "error": e.Error()})
			return
		}
		jsonReply(w, 200, value)
		return
	}
	var body execRequest
	dec := json.NewDecoder(r.Body)
	dec.DisallowUnknownFields()
	if err := dec.Decode(&body); err != nil {
		jsonReply(w, 400, map[string]any{"ok": false, "error": "invalid exec request"})
		return
	}
	var extra any
	if dec.Decode(&extra) != io.EOF || strings.TrimSpace(body.Cmd) == "" || strings.ContainsRune(body.Cmd, 0) {
		jsonReply(w, 400, map[string]any{"ok": false, "error": "invalid exec request"})
		return
	}
	jsonReply(w, 200, execute(r.Context(), body, s.dir, s.env))
}

type peerListener struct {
	*net.UnixListener
	uid   uint32
	slots chan struct{}
}
type peerConn struct {
	net.Conn
	once    sync.Once
	release func()
}

func (c *peerConn) Close() error { e := c.Conn.Close(); c.once.Do(c.release); return e }

func peer(c *net.UnixConn) (uint32, error) {
	raw, err := c.SyscallConn()
	if err != nil {
		return 0, err
	}
	var cred *syscall.Ucred
	var inner error
	err = raw.Control(func(fd uintptr) {
		cred, inner = syscall.GetsockoptUcred(int(fd), syscall.SOL_SOCKET, syscall.SO_PEERCRED)
	})
	if err != nil {
		return 0, err
	}
	if inner != nil {
		return 0, inner
	}
	if cred == nil {
		return 0, errors.New("missing peer")
	}
	return cred.Uid, nil
}

func (l *peerListener) Accept() (net.Conn, error) {
	for {
		c, err := l.AcceptUnix()
		if err != nil {
			return nil, err
		}
		uid, err := peer(c)
		if err != nil || uid != l.uid {
			c.Close()
			continue
		}
		select {
		case l.slots <- struct{}{}:
			return &peerConn{Conn: c, release: func() { <-l.slots }}, nil
		default:
			c.Close()
		}
	}
}

func startupCheck(uid, euid int, args []string, lookup func(string) (string, bool)) error {
	if uid != desktopUID || euid != desktopUID || len(args) != 1 {
		return errors.New("requires desktop uid, no arguments")
	}
	for _, key := range []string{"KVMBOX_OWNER_PASSWORD", "KVMBOX_ACCESS_SHA256", "KVMBOX_AUTOMATION_KEY"} {
		if _, ok := lookup(key); ok {
			return errors.New("inherited controller credential refused")
		}
	}
	return nil
}

func run() error {
	if e := startupCheck(os.Getuid(), os.Geteuid(), os.Args, os.LookupEnv); e != nil {
		return e
	}
	// The measured launcher must create this directory before dropping UID.
	st, err := os.Lstat(socketDir)
	if err != nil {
		return err
	}
	stat, ok := st.Sys().(*syscall.Stat_t)
	if !ok || !st.IsDir() || st.Mode().Perm() != 0750 || stat.Uid != desktopUID || stat.Gid != desktopUID {
		return errors.New("automation directory refused")
	}
	if filepath.Clean(socketPath) != socketPath {
		return errors.New("invalid fixed socket")
	}
	// Never unlink an existing socket: a duplicate service must fail closed.
	ln, err := net.ListenUnix("unix", &net.UnixAddr{Name: socketPath, Net: "unix"})
	if err != nil {
		return err
	}
	defer ln.Close()
	if err = os.Chmod(socketPath, 0660); err != nil {
		return err
	}
	ctx, stop := signal.NotifyContext(context.Background(), syscall.SIGTERM, syscall.SIGINT)
	defer stop()
	s := &service{dir: "/home/desktop", env: desktopEnvironment, slots: make(chan struct{}, 2), computerSlots: make(chan struct{}, 1)}
	srv := &http.Server{Handler: s, ReadHeaderTimeout: 3 * time.Second, IdleTimeout: 5 * time.Second, MaxHeaderBytes: 8192, BaseContext: func(net.Listener) context.Context { return ctx }}
	srv.SetKeepAlivesEnabled(false)
	go func() { <-ctx.Done(); _ = srv.Close() }()
	err = srv.Serve(&peerListener{UnixListener: ln, uid: clientUID, slots: make(chan struct{}, maxConnections)})
	if errors.Is(err, http.ErrServerClosed) {
		return nil
	}
	return err
}

func main() {
	if err := run(); err != nil {
		fmt.Fprintln(os.Stderr, "desktop automation:", err)
		os.Exit(1)
	}
}
