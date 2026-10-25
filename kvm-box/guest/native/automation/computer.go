//go:build linux

package main

import (
	"bytes"
	"context"
	"crypto/sha256"
	"encoding/base64"
	"encoding/binary"
	"encoding/json"
	"errors"
	"fmt"
	"image"
	"image/color"
	"image/jpeg"
	"io"
	"math"
	"os/exec"
	"strconv"
	"strings"
	"syscall"
	"time"
	"unicode/utf8"
)

type screen struct {
	pixels              []byte
	width, height, x, y int
}

func (s screen) ColorModel() color.Model { return color.RGBAModel }
func (s screen) Bounds() image.Rectangle { return image.Rect(0, 0, s.width, s.height) }
func (s screen) At(x, y int) color.Color {
	if x < 0 || y < 0 || x >= s.width || y >= s.height {
		return color.RGBA{}
	}
	i := (y*s.width + x) * 3
	return color.RGBA{s.pixels[i], s.pixels[i+1], s.pixels[i+2], 255}
}
func parseCapture(b []byte) (screen, error) {
	if len(b) < 20 || string(b[:4]) != "RGB1" {
		return screen{}, errors.New("invalid screenshot")
	}
	w, h := int(binary.BigEndian.Uint32(b[4:8])), int(binary.BigEndian.Uint32(b[8:12]))
	if w < 1 || h < 1 || w > 3840 || h > 2160 || len(b) != 20+w*h*3 {
		return screen{}, errors.New("invalid screenshot dimensions")
	}
	return screen{b[20:], w, h, int(int32(binary.BigEndian.Uint32(b[12:16]))), int(int32(binary.BigEndian.Uint32(b[16:20])))}, nil
}

func tool(ctx context.Context, path string, args []string, env []string, limit int) ([]byte, error) {
	ctx, cancel := context.WithTimeout(ctx, 3*time.Second)
	defer cancel()
	cmd := exec.CommandContext(ctx, path, args...)
	cmd.Env = env
	cmd.Dir = "/home/desktop"
	cmd.SysProcAttr = &syscall.SysProcAttr{Setpgid: true}
	cmd.Cancel = func() error {
		if cmd.Process != nil {
			return syscall.Kill(-cmd.Process.Pid, syscall.SIGKILL)
		}
		return nil
	}
	cmd.WaitDelay = 100 * time.Millisecond
	out := &boundedOutput{limit: limit}
	cmd.Stdout = out
	cmd.Stderr = io.Discard
	err := cmd.Run()
	if err != nil || out.truncated {
		return nil, errors.New("desktop operation failed")
	}
	return out.data, nil
}
func capture(ctx context.Context) (screen, error) {
	b, e := tool(ctx, "/usr/local/libexec/kvm-box/capture", nil, desktopEnvironment, 20+3840*2160*3)
	if e != nil {
		return screen{}, e
	}
	return parseCapture(b)
}
func input(ctx context.Context, args []string) error {
	_, e := tool(ctx, "/usr/bin/xdotool", args, desktopEnvironment, 4096)
	return e
}

type computerPlan struct {
	action, did   string
	commands      [][]string
	wait          time.Duration
	fixed         bool
	quality       int
	grid          float64
	releaseButton string
}

func number(v any) (float64, bool) {
	var n float64
	var e error
	switch x := v.(type) {
	case float64:
		n = x
	case string:
		n, e = strconv.ParseFloat(strings.TrimSpace(x), 64)
	default:
		return 0, false
	}
	return n, e == nil && !math.IsNaN(n) && !math.IsInf(n, 0)
}
func numberOr(m map[string]any, k string, def float64) float64 {
	if n, ok := number(m[k]); ok {
		return n
	}
	return def
}
func text(m map[string]any, k, def string) string {
	if s, ok := m[k].(string); ok {
		return s
	}
	return def
}
func coordinate(m map[string]any, x, y, array string) ([2]float64, bool, error) {
	var p [2]float64
	if v, exists := m[array]; exists {
		a, ok := v.([]any)
		if !ok || len(a) != 2 {
			return p, false, errors.New("coordinate must contain two numbers")
		}
		m = map[string]any{x: a[0], y: a[1]}
	}
	vx, xok := m[x]
	vy, yok := m[y]
	if !xok && !yok {
		return p, false, nil
	}
	px, okx := number(vx)
	py, oky := number(vy)
	if !okx || !oky {
		return p, false, errors.New("both coordinates must be numbers")
	}
	return [2]float64{px, py}, true, nil
}
func keyName(raw string) (string, error) {
	aliases := map[string]string{"ctrl": "ctrl", "control": "ctrl", "ctl": "ctrl", "shift": "shift", "alt": "alt", "option": "alt", "super": "super", "win": "super", "cmd": "super", "command": "super", "enter": "Return", "return": "Return", "esc": "Escape", "escape": "Escape", "tab": "Tab", "backspace": "BackSpace", "delete": "Delete", "del": "Delete", "insert": "Insert", "home": "Home", "end": "End", "up": "Up", "down": "Down", "left": "Left", "right": "Right", "pageup": "Prior", "page_up": "Prior", "pagedown": "Next", "page_down": "Next", "space": "space", "spacebar": "space", "minus": "minus", "equal": "equal", "plus": "plus", "comma": "comma", "period": "period", "slash": "slash", "backslash": "backslash", "semicolon": "semicolon", "apostrophe": "apostrophe", "bracketleft": "bracketleft", "bracketright": "bracketright", "grave": "grave", "caps_lock": "Caps_Lock", "print": "Print", "pause": "Pause"}
	parts := strings.Split(strings.TrimSpace(raw), "+")
	if len(parts) > 8 {
		return "", errors.New("too many keys")
	}
	for i, p := range parts {
		p = strings.ToLower(strings.TrimSpace(p))
		if name, ok := aliases[p]; ok {
			parts[i] = name
			continue
		}
		if len(p) == 1 && ((p[0] >= 'a' && p[0] <= 'z') || (p[0] >= '0' && p[0] <= '9')) {
			parts[i] = p
			continue
		}
		if strings.HasPrefix(p, "f") {
			if n, e := strconv.Atoi(p[1:]); e == nil && n >= 1 && n <= 24 {
				parts[i] = "F" + strconv.Itoa(n)
				continue
			}
		}
		return "", errors.New("unsupported key name")
	}
	return strings.Join(parts, "+"), nil
}

// Plan every input before sending any event. Invalid later fields therefore
// cannot leave a partial click, drag, modifier or text entry on the desktop.
func planComputer(m map[string]any, s screen) (computerPlan, error) {
	p := computerPlan{action: strings.NewReplacer("-", "_", " ", "_").Replace(strings.ToLower(strings.TrimSpace(text(m, "action", "screenshot")))), quality: clamp(int(numberOr(m, "quality", 70)), 30, 95), wait: time.Duration(math.Max(0, math.Min(20000, numberOr(m, "wait_ms", 8000)))) * time.Millisecond}
	if g, exists := m["grid"]; exists {
		n, ok := number(g)
		if !ok || n < 1 || n > 1e9 {
			return p, errors.New("invalid coordinate grid")
		}
		p.grid = n
	}
	at, has, e := coordinate(m, "x", "y", "coordinate")
	if e != nil {
		return p, e
	}
	point := func(v [2]float64) []string {
		if p.grid > 0 {
			v[0] = v[0] / p.grid * float64(s.width)
			v[1] = v[1] / p.grid * float64(s.height)
		}
		x := int(math.Round(math.Max(0, math.Min(float64(s.width-1), v[0]))))
		y := int(math.Round(math.Max(0, math.Min(float64(s.height-1), v[1]))))
		// Debian xdotool 3.20160805 waits indefinitely with --sync if the
		// pointer is already at the target. XCloseDisplay flushes this request;
		// the subsequent capture/settle loop observes the resulting X11 state.
		return []string{"mousemove", strconv.Itoa(x), strconv.Itoa(y)}
	}
	button := "1"
	if b, exists := m["button"]; exists {
		name, ok := b.(string)
		if !ok {
			return p, errors.New("invalid mouse button")
		}
		switch strings.ToLower(strings.TrimSpace(name)) {
		case "left", "1":
			button = "1"
		case "middle", "2":
			button = "2"
		case "right", "3":
			button = "3"
		default:
			return p, errors.New("invalid mouse button")
		}
	}
	move := func() {
		if has {
			p.commands = append(p.commands, point(at))
		}
	}
	switch p.action {
	case "screenshot", "look", "observe":
		if _, ok := m["wait_ms"]; !ok {
			p.wait = 0
		}
		p.did = "took a screenshot"
	case "move", "mouse_move", "hover":
		if !has {
			return p, errors.New("move needs coordinates")
		}
		move()
		p.did = "moved the pointer"
	case "click", "left_click", "double_click", "triple_click", "right_click", "middle_click":
		n := clamp(int(numberOr(m, "count", 1)), 1, 3)
		switch p.action {
		case "double_click":
			n = 2
		case "triple_click":
			n = 3
		case "right_click":
			button = "3"
		case "middle_click":
			button = "2"
		}
		move()
		p.commands = append(p.commands, []string{"click", "--repeat", strconv.Itoa(n), "--delay", "100", button})
		p.did = "clicked the mouse"
	case "mouse_down", "left_mouse_down", "mouse_up", "left_mouse_up":
		move()
		cmd := "mouseup"
		if strings.HasSuffix(p.action, "down") {
			cmd = "mousedown"
		}
		p.commands = append(p.commands, []string{cmd, button})
		p.did = cmd
	case "drag", "left_click_drag":
		to, ok, e := coordinate(m, "to_x", "to_y", "to_coordinate")
		if e != nil || !ok || !has {
			return p, errors.New("drag needs start and end coordinates")
		}
		move()
		p.commands = append(p.commands, []string{"mousedown", button})
		p.releaseButton = button
		for i := 1; i <= 8; i++ {
			f := float64(i) / 8
			p.commands = append(p.commands, point([2]float64{at[0] + (to[0]-at[0])*f, at[1] + (to[1]-at[1])*f}))
		}
		p.commands = append(p.commands, []string{"mouseup", button})
		p.did = "dragged the pointer"
	case "scroll":
		dir := strings.ToLower(strings.TrimSpace(text(m, "direction", "down")))
		buttons := map[string]string{"up": "4", "down": "5", "left": "6", "right": "7"}
		b, ok := buttons[dir]
		if !ok {
			return p, errors.New("invalid scroll direction")
		}
		move()
		p.commands = append(p.commands, []string{"click", "--repeat", strconv.Itoa(clamp(int(numberOr(m, "amount", 3)), 1, 30)), "--delay", "20", b})
		p.did = "scrolled " + dir
	case "type", "type_text", "write":
		v, ok := m["text"].(string)
		if !ok || v == "" || utf8.RuneCountInString(v) > 800 || strings.ContainsRune(v, 0) {
			return p, errors.New("type requires 1..800 non-NUL characters")
		}
		p.commands = append(p.commands, []string{"type", "--clearmodifiers", "--delay", "0", "--", v})
		p.did = "typed text"
	case "key", "keys", "press", "hotkey", "key_press":
		keys, e := keyName(text(m, "keys", text(m, "key", text(m, "text", ""))))
		if e != nil {
			return p, e
		}
		p.commands = append(p.commands, []string{"key", "--clearmodifiers", keys})
		p.did = "pressed " + keys
	case "wait", "sleep":
		ms := numberOr(m, "ms", numberOr(m, "duration", numberOr(m, "wait_ms", 1000)/1000)*1000)
		p.wait = time.Duration(math.Max(0, math.Min(20000, ms))) * time.Millisecond
		p.fixed = true
		p.did = "waited"
	default:
		return p, errors.New("unsupported computer action")
	}
	return p, nil
}

type cappedJPEG struct{ bytes.Buffer }

func (b *cappedJPEG) Write(p []byte) (int, error) {
	if b.Len()+len(p) > 4<<20 {
		return 0, errors.New("screenshot exceeds response limit")
	}
	return b.Buffer.Write(p)
}

func computer(ctx context.Context, m map[string]any, grab func(context.Context) (screen, error), send func(context.Context, []string) error) (map[string]any, error) {
	ctx, cancel := context.WithTimeout(ctx, 30*time.Second)
	defer cancel()
	before, e := grab(ctx)
	if e != nil {
		return nil, e
	}
	p, e := planComputer(m, before)
	if e != nil {
		return nil, e
	}
	if p.releaseButton != "" {
		defer func() {
			cleanup, cancel := context.WithTimeout(context.Background(), time.Second)
			defer cancel()
			_ = send(cleanup, []string{"mouseup", p.releaseButton})
		}()
	}
	for _, cmd := range p.commands {
		if e = send(ctx, cmd); e != nil {
			return nil, e
		}
	}
	started := time.Now()
	lastChange := started
	last := sha256.Sum256(before.pixels)
	original := last
	current := before
	settled := p.wait == 0
	changed := false
	for time.Since(started) < p.wait {
		delay := 100 * time.Millisecond
		if left := p.wait - time.Since(started); left < delay {
			delay = left
		}
		timer := time.NewTimer(delay)
		select {
		case <-ctx.Done():
			timer.Stop()
			return nil, ctx.Err()
		case <-timer.C:
		}
		current, e = grab(ctx)
		if e != nil {
			return nil, e
		}
		h := sha256.Sum256(current.pixels)
		if h != original {
			changed = true
		}
		if h != last {
			last = h
			lastChange = time.Now()
		}
		// Pixel stability, not a claim that the desktop's CPU is idle.
		if !p.fixed && time.Since(started) >= 600*time.Millisecond && time.Since(lastChange) >= 500*time.Millisecond {
			settled = true
			break
		}
	}
	if len(p.commands) > 0 && p.wait == 0 {
		current, e = grab(ctx)
		if e != nil {
			return nil, e
		}
		changed = sha256.Sum256(current.pixels) != original
	}
	if p.fixed {
		settled = true
	}
	var jpegBytes cappedJPEG
	if e = jpeg.Encode(&jpegBytes, current, &jpeg.Options{Quality: p.quality}); e != nil {
		return nil, e
	}
	coords := fmt.Sprintf("pixels: x 0..%d from the left, y 0..%d from the top", current.width-1, current.height-1)
	x, y := current.x, current.y
	if p.grid > 0 {
		coords = fmt.Sprintf("0..%g on each axis, from the top-left", p.grid)
		x = int(math.Round(float64(x) / float64(current.width) * p.grid))
		y = int(math.Round(float64(y) / float64(current.height) * p.grid))
	}
	var cursor any
	if current.x >= 0 && current.y >= 0 {
		cursor = map[string]int{"x": x, "y": y}
	}
	var change any
	if len(p.commands) > 0 {
		change = changed
	}
	return map[string]any{"ok": true, "action": p.action, "did": p.did, "screen": map[string]int{"width": current.width, "height": current.height}, "coordinates": coords, "cursor": cursor, "settled": settled, "changed": change, "waited_ms": time.Since(started).Milliseconds(), "mime": "image/jpeg", "image": base64.StdEncoding.EncodeToString(jpegBytes.Bytes())}, nil
}

func decodeComputer(r io.Reader) (map[string]any, error) {
	dec := json.NewDecoder(r)
	var m map[string]any
	if e := dec.Decode(&m); e != nil {
		return nil, e
	}
	if m == nil {
		return nil, errors.New("expected object")
	}
	var extra any
	if dec.Decode(&extra) != io.EOF {
		return nil, errors.New("trailing request data")
	}
	return m, nil
}
