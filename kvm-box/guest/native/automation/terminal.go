//go:build linux

// The owner's terminal: one interactive shell on a pseudo-terminal, run as the
// desktop user with the same constructed environment as /exec. It is the same
// authority as /exec (owner-authorized desktop-user commands), kept as one
// long-lived session so the dashboard can show a real terminal. Every request
// is a bounded JSON exchange; output is polled from a fixed-size scrollback by
// an absolute cursor, so a page that reconnects sees what it missed.
package main

import (
	"context"
	"encoding/base64"
	"encoding/json"
	"errors"
	"io"
	"os"
	"os/exec"
	"strconv"
	"sync"
	"syscall"
	"time"
	"unsafe"
)

const (
	terminalScrollback = 256 << 10
	terminalChunk      = 128 << 10
	terminalMaxInput   = 16 << 10
	terminalMaxWait    = 20 * time.Second
	terminalWriteWait  = 2 * time.Second
	terminalRespawn    = time.Second
	terminalExited     = "\r\n[shell exited]\r\n"
)

var terminalShells = [][]string{{"/bin/bash"}, {"/bin/sh"}}

var (
	errTerminalRestarting = errors.New("terminal restarting")
	errInvalidTerminal    = errors.New("invalid terminal request")
)

type terminalRequest struct {
	Op     string `json:"op"`
	Data   string `json:"data,omitempty"`
	Cursor *int64 `json:"cursor,omitempty"`
	WaitMS *int   `json:"wait_ms,omitempty"`
	Cols   *int   `json:"cols,omitempty"`
	Rows   *int   `json:"rows,omitempty"`
}

type terminalResult struct {
	OK      bool   `json:"ok"`
	Cursor  int64  `json:"cursor"`
	Data    string `json:"data"`
	Reset   bool   `json:"reset"`
	Running bool   `json:"running"`
	Cols    int    `json:"cols"`
	Rows    int    `json:"rows"`
	Error   string `json:"error,omitempty"`
}

type winsize struct{ Row, Col, X, Y uint16 }

type terminalSession struct {
	master *os.File
	pid    int
	exited bool
}

type terminal struct {
	dir    string
	env    []string
	shells [][]string

	write sync.Mutex // one writer at a time; never held with mu

	mu      sync.Mutex
	cur     *terminalSession
	started time.Time
	buf     []byte // the newest terminalScrollback bytes of output
	end     int64  // absolute offset just past buf
	cols    int
	rows    int
	changed chan struct{} // closed, then replaced, whenever buf or cur changes
	closed  bool
}

func newTerminal(dir string, env []string) *terminal {
	env = append(append([]string(nil), env...), "TERM=xterm-256color", "SHELL=/bin/bash", "USER=desktop", "LOGNAME=desktop")
	return &terminal{dir: dir, env: env, shells: terminalShells, cols: 100, rows: 26, changed: make(chan struct{})}
}

func ptyControl(f *os.File, request uintptr, arg unsafe.Pointer) error {
	raw, err := f.SyscallConn()
	if err != nil {
		return err
	}
	var inner syscall.Errno
	if err = raw.Control(func(fd uintptr) {
		_, _, inner = syscall.Syscall(syscall.SYS_IOCTL, fd, request, uintptr(arg))
	}); err != nil {
		return err
	}
	if inner != 0 {
		return inner
	}
	return nil
}

// openPTY returns the nonblocking master (so deadlines and Close interrupt
// its reader) and the slave the shell is given as its controlling terminal.
func openPTY(cols, rows int) (master, slave *os.File, err error) {
	fd, err := syscall.Open("/dev/ptmx", syscall.O_RDWR|syscall.O_NOCTTY|syscall.O_CLOEXEC|syscall.O_NONBLOCK, 0)
	if err != nil {
		return nil, nil, err
	}
	master = os.NewFile(uintptr(fd), "/dev/ptmx")
	defer func() {
		if err != nil {
			master.Close()
		}
	}()
	var unlock int32
	if err = ptyControl(master, syscall.TIOCSPTLCK, unsafe.Pointer(&unlock)); err != nil {
		return nil, nil, err
	}
	var number uint32
	if err = ptyControl(master, syscall.TIOCGPTN, unsafe.Pointer(&number)); err != nil {
		return nil, nil, err
	}
	size := winsize{Row: uint16(rows), Col: uint16(cols)}
	if err = ptyControl(master, syscall.TIOCSWINSZ, unsafe.Pointer(&size)); err != nil {
		return nil, nil, err
	}
	slave, err = os.OpenFile("/dev/pts/"+strconv.FormatUint(uint64(number), 10), os.O_RDWR|syscall.O_NOCTTY, 0)
	if err != nil {
		return nil, nil, err
	}
	return master, slave, nil
}

// Caller holds mu.
func (t *terminal) append(p []byte) {
	t.buf = append(t.buf, p...)
	if extra := len(t.buf) - terminalScrollback; extra > 0 {
		t.buf = append(t.buf[:0], t.buf[extra:]...)
	}
	t.end += int64(len(p))
	t.wake()
}

// Caller holds mu.
func (t *terminal) wake() {
	close(t.changed)
	t.changed = make(chan struct{})
}

// ensure starts a shell when none runs. A shell that keeps exiting at once is
// started at most once per terminalRespawn. Caller holds mu.
func (t *terminal) ensure() error {
	if t.closed {
		return errors.New("terminal closed")
	}
	if t.cur != nil {
		return nil
	}
	if !t.started.IsZero() && time.Since(t.started) < terminalRespawn {
		return errTerminalRestarting
	}
	t.started = time.Now()
	var shell []string
	for _, candidate := range t.shells {
		if st, err := os.Stat(candidate[0]); err == nil && st.Mode().IsRegular() && st.Mode().Perm()&0o111 != 0 {
			shell = candidate
			break
		}
	}
	if shell == nil {
		return errors.New("no shell")
	}
	master, slave, err := openPTY(t.cols, t.rows)
	if err != nil {
		return errors.New("no pseudo-terminal")
	}
	cmd := exec.Command(shell[0], shell[1:]...)
	cmd.Dir = t.dir
	cmd.Env = append([]string(nil), t.env...)
	cmd.Stdin, cmd.Stdout, cmd.Stderr = slave, slave, slave
	// A new session whose controlling terminal is the slave: job control works
	// and closing the master hangs up everything still attached to it.
	cmd.SysProcAttr = &syscall.SysProcAttr{Setsid: true, Setctty: true, Ctty: 0}
	err = cmd.Start()
	slave.Close()
	if err != nil {
		master.Close()
		return errors.New("shell could not start")
	}
	s := &terminalSession{master: master, pid: cmd.Process.Pid}
	t.cur = s
	t.wake()
	go func() {
		_ = cmd.Wait()
		t.mu.Lock()
		s.exited = true
		t.mu.Unlock()
		// Let the reader collect what the shell wrote last, but do not wait on
		// background jobs which still hold the slave open.
		if master.SetReadDeadline(time.Now().Add(200*time.Millisecond)) != nil {
			master.Close()
		}
	}()
	go func() {
		chunk := make([]byte, 32<<10)
		for {
			n, err := master.Read(chunk)
			t.mu.Lock()
			if n > 0 {
				t.append(chunk[:n])
			}
			if err != nil {
				master.Close()
				if t.cur == s {
					t.cur = nil
					t.append([]byte(terminalExited))
				}
				t.mu.Unlock()
				return
			}
			t.mu.Unlock()
		}
	}()
	return nil
}

// hangup ends the current shell and everything attached to its terminal.
// Caller holds mu.
func (t *terminal) hangup() {
	s := t.cur
	if s == nil {
		return
	}
	t.cur = nil
	if !s.exited {
		_ = syscall.Kill(-s.pid, syscall.SIGHUP)
	}
	s.master.Close()
	t.append([]byte(terminalExited))
	time.AfterFunc(time.Second, func() {
		t.mu.Lock()
		defer t.mu.Unlock()
		if !s.exited {
			_ = syscall.Kill(-s.pid, syscall.SIGKILL)
		}
	})
}

func (t *terminal) close() {
	t.mu.Lock()
	defer t.mu.Unlock()
	t.hangup()
	t.closed = true
	t.wake()
}

// Caller holds mu.
func (t *terminal) result() terminalResult {
	return terminalResult{OK: true, Cursor: t.end, Running: t.cur != nil, Cols: t.cols, Rows: t.rows}
}

func (t *terminal) read(ctx context.Context, cursor *int64, wait time.Duration) (terminalResult, error) {
	timer := time.NewTimer(wait)
	defer timer.Stop()
	for {
		t.mu.Lock()
		// A page polls for as long as it shows the terminal, so reading is what
		// keeps a shell there. A refused respawn is retried on the next read.
		err := t.ensure()
		start := t.end - int64(len(t.buf))
		from, reset := start, cursor == nil
		if cursor != nil {
			switch {
			case *cursor > t.end || *cursor < 0: // another service instance's cursor
				reset = true
			case *cursor >= start:
				from = *cursor
			default: // scrolled out
				reset = true
			}
		}
		if from < t.end || reset {
			data := t.buf[from-start:]
			if len(data) > terminalChunk {
				data = data[:terminalChunk]
			}
			r := t.result()
			r.Cursor, r.Data, r.Reset = from+int64(len(data)), base64.StdEncoding.EncodeToString(data), reset
			t.mu.Unlock()
			return r, nil
		}
		if err != nil && err != errTerminalRestarting {
			t.mu.Unlock()
			return terminalResult{}, err
		}
		changed := t.changed
		r := t.result()
		t.mu.Unlock()
		var respawn <-chan time.Time
		if err != nil {
			respawn = time.After(terminalRespawn / 4)
		}
		select {
		case <-changed:
		case <-respawn:
		case <-timer.C:
			return r, nil
		case <-ctx.Done():
			return r, nil
		}
	}
}

func (t *terminal) input(data []byte) (terminalResult, error) {
	t.write.Lock()
	defer t.write.Unlock()
	t.mu.Lock()
	if err := t.ensure(); err != nil {
		t.mu.Unlock()
		return terminalResult{}, err
	}
	s, r := t.cur, t.result()
	t.mu.Unlock()
	// A full input queue (nothing is reading the terminal) must not hold the
	// request: the write is bounded and the rest is dropped.
	_ = s.master.SetWriteDeadline(time.Now().Add(terminalWriteWait))
	if _, err := s.master.Write(data); err != nil {
		return terminalResult{}, errors.New("terminal is not accepting input")
	}
	return r, nil
}

func (t *terminal) resize(cols, rows int) (terminalResult, error) {
	t.mu.Lock()
	defer t.mu.Unlock()
	t.cols, t.rows = cols, rows
	if t.cur != nil {
		size := winsize{Row: uint16(rows), Col: uint16(cols)}
		if err := ptyControl(t.cur.master, syscall.TIOCSWINSZ, unsafe.Pointer(&size)); err != nil {
			return terminalResult{}, errors.New("terminal could not be resized")
		}
	}
	return t.result(), nil
}

func (t *terminal) restart() (terminalResult, error) {
	t.mu.Lock()
	defer t.mu.Unlock()
	t.hangup()
	t.started = time.Time{}
	if err := t.ensure(); err != nil {
		return terminalResult{}, err
	}
	return t.result(), nil
}

func decodeTerminal(r io.Reader) (terminalRequest, error) {
	var body terminalRequest
	dec := json.NewDecoder(r)
	dec.DisallowUnknownFields()
	if err := dec.Decode(&body); err != nil {
		return body, err
	}
	var extra any
	if dec.Decode(&extra) != io.EOF {
		return body, errors.New("trailing data")
	}
	return body, nil
}

// serve validates the whole request before the terminal is touched.
func (t *terminal) serve(ctx context.Context, body terminalRequest) (terminalResult, error) {
	bad := errInvalidTerminal
	only := func(data, cursor, wait, size bool) bool {
		return (data || body.Data == "") && (cursor || body.Cursor == nil) && (wait || body.WaitMS == nil) &&
			(size || (body.Cols == nil && body.Rows == nil))
	}
	switch body.Op {
	case "read":
		if !only(false, true, true, false) {
			return terminalResult{}, bad
		}
		wait := time.Duration(0)
		if body.WaitMS != nil {
			wait = time.Duration(clamp(*body.WaitMS, 0, int(terminalMaxWait/time.Millisecond))) * time.Millisecond
		}
		return t.read(ctx, body.Cursor, wait)
	case "write":
		data, err := base64.StdEncoding.DecodeString(body.Data)
		if !only(true, false, false, false) || err != nil || len(data) == 0 || len(data) > terminalMaxInput {
			return terminalResult{}, bad
		}
		return t.input(data)
	case "resize":
		if !only(false, false, false, true) || body.Cols == nil || body.Rows == nil ||
			*body.Cols < 20 || *body.Cols > 400 || *body.Rows < 5 || *body.Rows > 200 {
			return terminalResult{}, bad
		}
		return t.resize(*body.Cols, *body.Rows)
	case "restart":
		if !only(false, false, false, false) {
			return terminalResult{}, bad
		}
		return t.restart()
	}
	return terminalResult{}, bad
}
