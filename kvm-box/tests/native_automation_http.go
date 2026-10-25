//go:build ignore

// Disposable VM acceptance client for the actual outer->inner->automation path.
// A fixture supplies controller UID5001; this is NOT owner authentication proof.
package main

import (
	"bytes"
	"encoding/base64"
	"encoding/json"
	"fmt"
	"image/jpeg"
	"io"
	"net/http"
	"os"
	"strings"
	"time"
)

var client = &http.Client{Timeout: 135 * time.Second, Transport: &http.Transport{Proxy: nil}, CheckRedirect: func(*http.Request, []*http.Request) error { return fmt.Errorf("redirect refused") }}

func require(ok bool, label string) {
	if !ok {
		panic(label)
	}
}
func call(path string, body any, expected int) map[string]any {
	b, e := json.Marshal(body)
	if e != nil {
		panic(e)
	}
	req, e := http.NewRequest("POST", "http://127.0.0.1:19080"+path, bytes.NewReader(b))
	if e != nil {
		panic(e)
	}
	req.Header.Set("Content-Type", "application/json")
	req.Header.Set("X-KVM-Box-Request", "1")
	resp, e := client.Do(req)
	if e != nil {
		panic(e)
	}
	defer resp.Body.Close()

	b, e = io.ReadAll(io.LimitReader(resp.Body, 7<<20))
	if e != nil {
		panic(e)
	}
	require(len(b) < 7<<20, "response cap")
	var v map[string]any
	require(json.Unmarshal(b, &v) == nil, "response JSON")
	if resp.StatusCode != expected {
		if path == "/computer" {
			diagnostic := command("xdotool version; xdotool getmouselocation --shell; timeout 4 xdotool mousemove --sync 123 234; printf ' first=%s\\n' \"$?\"; timeout 4 xdotool mousemove --sync 123 234; printf ' repeat=%s\\n' \"$?\"", 12, 4096)
			panic(fmt.Sprintf("%s status %d error=%v x11=%v", path, resp.StatusCode, v["error"], diagnostic))
		}
		panic(fmt.Sprintf("%s status %d error=%v", path, resp.StatusCode, v["error"]))
	}
	return v
}
func command(cmd string, seconds, cap int) map[string]any {
	return call("/exec", map[string]any{"cmd": cmd, "timeout_s": seconds, "max_bytes": cap}, 200)
}
func success(v map[string]any, output string) {
	require(v["ok"] == true && v["exitCode"] == float64(0) && v["output"] == output && v["truncated"] == false, "command result")
}
func picture(v map[string]any) (int, int) {
	require(v["ok"] == true && v["mime"] == "image/jpeg", "picture response")
	encoded, ok := v["image"].(string)
	require(ok, "picture field")
	b, e := base64.StdEncoding.Strict().DecodeString(encoded)
	require(e == nil && len(b) <= 4<<20, "picture encoding")
	img, e := jpeg.Decode(bytes.NewReader(b))
	require(e == nil, "full JPEG decode")
	screen, ok := v["screen"].(map[string]any)
	require(ok, "screen metadata")
	w, h := img.Bounds().Dx(), img.Bounds().Dy()
	require(w > 0 && h > 0 && screen["width"] == float64(w) && screen["height"] == float64(h), "pixel dimensions")
	return w, h
}
func main() {
	require(len(os.Args) == 2 && (os.Args[1] == "allow" || os.Args[1] == "deny"), "fixture argument")
	if os.Args[1] == "deny" {
		require(os.Getuid() >= 1000 && os.Getuid() <= 1003, "fixture denied identity")
		client.Timeout = 2 * time.Second
		req, _ := http.NewRequest("POST", "http://127.0.0.1:19080/exec", strings.NewReader(`{"cmd":"true"}`))
		resp, e := client.Do(req)
		if resp != nil {
			resp.Body.Close()
		}
		require(e != nil, "noncontroller reached gateway")
		fmt.Println(`{"passed":true,"networkDenied":true}`)
		return
	}
	require(os.Getuid() == 5001, "fixture controller identity")
	checks := []string{}
	success(command("id -u; id -g", 5, 1024), "1000\n1000\n")
	checks = append(checks, "desktop identity")
	success(command(`python3 -I -c 'import os; assert all(k not in os.environ for k in ["KVMBOX_OWNER_PASSWORD","KVMBOX_AUTOMATION_KEY","KVMBOX_ACCESS_SHA256"]); assert not os.path.exists("/dev/dri") and not os.path.exists("/dev/nvidia0"); print("boundaries-ok")'`, 5, 1024), "boundaries-ok\n")
	checks = append(checks, "no credentials or physical GPU nodes")
	v := command("printf command-failed; exit 7", 5, 1024)
	require(v["ok"] == true && v["exitCode"] == float64(7) && v["output"] == "command-failed", "nonzero exit")
	checks = append(checks, "nonzero exit")
	v = command("python3 -I -c 'print(\"a\"*10000)'", 5, 1024)
	require(v["ok"] == true && v["truncated"] == true && len(v["output"].(string)) == 1024, "bounded output")
	checks = append(checks, "bounded output")
	v = command("sleep 3; printf timeout-failed", 1, 1024)
	require(v["ok"] == false && !strings.Contains(v["output"].(string), "timeout-failed"), "timeout")
	checks = append(checks, "timeout")
	success(command("sleep 2; pgrep -u 1000 -x sleep >/dev/null && exit 9; printf cleanup-ok", 5, 1024), "cleanup-ok")
	checks = append(checks, "timeout descendant cleanup")
	v = call("/computer", map[string]any{"action": "screenshot", "quality": 60}, 200)
	w, h := picture(v)
	checks = append(checks, "real JPEG decoded")
	v = call("/computer", map[string]any{"action": "move", "x": w / 2, "y": h / 2}, 200)
	picture(v)
	success(command("xdotool getmouselocation --shell | head -n 2", 5, 1024), fmt.Sprintf("X=%d\nY=%d\n", w/2, h/2))
	checks = append(checks, "input confirmed independently by X11")
	for _, point := range [][2]int{{123, 234}, {123, 234}, {w / 2, h / 2}, {w / 2, h / 2}} {
		picture(call("/computer", map[string]any{"action": "move", "x": point[0], "y": point[1]}, 200))
		success(command("xdotool getmouselocation --shell | head -n 2", 5, 1024), fmt.Sprintf("X=%d\nY=%d\n", point[0], point[1]))
	}
	checks = append(checks, "repeated same-coordinate movement")
	v = call("/computer", map[string]any{"action": "unsupported-action"}, 400)
	require(v["ok"] == false, "invalid action refusal")
	checks = append(checks, "invalid action refused")
	success(command("sleep 65; printf long-command-ok", 70, 1024), "long-command-ok")
	checks = append(checks, "response survives 60-second gateway default")
	b, _ := json.Marshal(map[string]any{"passed": true, "checks": checks, "screen": map[string]int{"width": w, "height": h}, "ownerAuthentication": false, "productionRelease": false})
	fmt.Println(string(b))
}
