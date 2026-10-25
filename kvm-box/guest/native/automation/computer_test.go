//go:build linux

package main

import (
	"bytes"
	"context"
	"encoding/base64"
	"encoding/binary"
	"errors"
	"image/jpeg"
	"net/http/httptest"
	"reflect"
	"strings"
	"testing"
	"time"
)

func testScreen() screen {
	return screen{pixels: bytes.Repeat([]byte{220, 30, 10}, 960*600), width: 960, height: 600, x: 480, y: 300}
}
func TestCaptureFraming(t *testing.T) {
	b := make([]byte, 26)
	copy(b, "RGB1")
	binary.BigEndian.PutUint32(b[4:], 2)
	binary.BigEndian.PutUint32(b[8:], 1)
	copy(b[20:], []byte{255, 0, 0, 0, 255, 0})
	s, e := parseCapture(b)
	if e != nil || s.width != 2 || s.height != 1 {
		t.Fatal(s, e)
	}
	for _, bad := range [][]byte{nil, b[:25], append(append([]byte(nil), b...), 0), bytes.Repeat([]byte{255}, 26)} {
		if _, e := parseCapture(bad); e == nil {
			t.Fatal("invalid capture accepted")
		}
	}
	binary.BigEndian.PutUint32(b[4:], 0xffffffff)
	if _, e := parseCapture(b); e == nil {
		t.Fatal("overflow width accepted")
	}
}

func TestGridAndInputPlan(t *testing.T) {
	s := testScreen()
	p, e := planComputer(map[string]any{"action": "click", "x": "500", "y": float64(500), "grid": float64(1000)}, s)
	if e != nil || !reflect.DeepEqual(p.commands[0], []string{"mousemove", "480", "300"}) {
		t.Fatal(p, e)
	}
	p, e = planComputer(map[string]any{"action": "type", "text": "--file /etc/passwd; $(echo bad)\n"}, s)
	if e != nil || p.commands[0][4] != "--" || p.commands[0][5] != "--file /etc/passwd; $(echo bad)\n" {
		t.Fatal("text interpreted as command syntax", p, e)
	}
	p, e = planComputer(map[string]any{"action": "key", "keys": "ctrl+L"}, s)
	if e != nil || p.commands[0][2] != "ctrl+l" {
		t.Fatal(p, e)
	}
	for _, m := range []map[string]any{
		{"action": "key", "keys": "ctrl+--window"},
		{"action": "drag", "x": float64(1), "y": float64(2)},
		{"action": "click", "x": float64(1)},
		{"action": "click", "coordinate": []any{float64(1)}},
		{"action": "scroll", "direction": "invalid"},
		{"action": "type", "text": strings.Repeat("x", 801)},
		{"action": "screenshot", "grid": "NaN"},
	} {
		sent := false
		_, e := computer(context.Background(), m, func(context.Context) (screen, error) { return s, nil }, func(context.Context, []string) error { sent = true; return nil })
		if e == nil || sent {
			t.Fatal("bad request caused input", m, e)
		}
	}
}

func TestScreenshotWireCompatibility(t *testing.T) {
	s := testScreen()
	r, e := computer(context.Background(), map[string]any{"grid": float64(1000)}, func(context.Context) (screen, error) { return s, nil }, func(context.Context, []string) error { t.Fatal("screenshot sent input"); return nil })
	if e != nil {
		t.Fatal(e)
	}
	if r["mime"] != "image/jpeg" || r["changed"] != nil || r["settled"] != true || !reflect.DeepEqual(r["cursor"], map[string]int{"x": 500, "y": 500}) {
		t.Fatal("incorrect response metadata", r["cursor"])
	}
	b, e := base64.StdEncoding.DecodeString(r["image"].(string))
	if e != nil {
		t.Fatal(e)
	}
	im, e := jpeg.Decode(bytes.NewReader(b))
	if e != nil || im.Bounds().Dx() != 960 || im.Bounds().Dy() != 600 {
		t.Fatal("invalid screenshot JPEG", e)
	}
}

func TestDragFailureReleasesButton(t *testing.T) {
	s := testScreen()
	var commands [][]string
	_, e := computer(context.Background(), map[string]any{"action": "drag", "x": float64(1), "y": float64(2), "to_x": float64(30), "to_y": float64(40)}, func(context.Context) (screen, error) { return s, nil }, func(_ context.Context, args []string) error {
		commands = append(commands, args)
		if len(commands) == 3 {
			return errors.New("input failed")
		}
		return nil
	})
	if e == nil || !reflect.DeepEqual(commands[len(commands)-1], []string{"mouseup", "1"}) {
		t.Fatal("failed drag left button down", commands, e)
	}
}

func TestPixelSettleAndWaitCancellation(t *testing.T) {
	s := testScreen()
	reads := 0
	r, e := computer(context.Background(), map[string]any{"action": "click"}, func(context.Context) (screen, error) {
		reads++
		if reads == 2 {
			s.pixels = bytes.Repeat([]byte{10, 30, 220}, 960*600)
		}
		return s, nil
	}, func(context.Context, []string) error { return nil })
	if e != nil || r["settled"] != true || r["changed"] != true {
		t.Fatal("did not report settled changed pixels", e)
	}
	ctx, cancel := context.WithTimeout(context.Background(), 25*time.Millisecond)
	defer cancel()
	_, e = computer(ctx, map[string]any{"action": "wait", "ms": float64(20000)}, func(context.Context) (screen, error) { return s, nil }, func(context.Context, []string) error { return nil })
	if !errors.Is(e, context.DeadlineExceeded) {
		t.Fatal("wait ignored cancellation", e)
	}
}

func TestComputerHTTP(t *testing.T) {
	s := testService(t)
	s.computerSlots = make(chan struct{}, 1)
	s.grab = func(context.Context) (screen, error) { return testScreen(), nil }
	s.send = func(context.Context, []string) error { return nil }
	w := httptest.NewRecorder()
	s.ServeHTTP(w, httptest.NewRequest("POST", "/computer", strings.NewReader(`{"action":"screenshot"}`)))
	if w.Code != 200 || !strings.Contains(w.Body.String(), `"mime":"image/jpeg"`) {
		t.Fatal("computer HTTP failed", w.Code)
	}
	s.computerSlots <- struct{}{}
	w = httptest.NewRecorder()
	s.ServeHTTP(w, httptest.NewRequest("POST", "/computer", strings.NewReader(`{"action":"click"}`)))
	if w.Code != 429 {
		t.Fatal("overlapping computer input accepted")
	}
}
