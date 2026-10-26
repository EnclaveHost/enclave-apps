//go:build linux

package main

import (
	"bufio"
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net"
	"net/http"
	"time"
)

const maxAutomationRequest = 64 << 10
const maxAutomationResponse = 7 << 20

func (g *gateway) serveAutomation(w http.ResponseWriter, r *http.Request) {
	if !jsonContentType(r.Header) {
		writeError(w, http.StatusUnsupportedMediaType)
		return
	}
	body, code := readBody(r, maxAutomationRequest)
	if code != 0 {
		writeError(w, code)
		return
	}
	if _, ok := strictObject(body, maxJSONDepth); !ok {
		writeError(w, http.StatusBadRequest)
		return
	}
	deadline := time.Now().Add(g.cfg.automationTimeout)
	rc := http.NewResponseController(w)
	_ = rc.SetReadDeadline(deadline.Add(deadlineSlack))
	_ = rc.SetWriteDeadline(deadline.Add(deadlineSlack))
	status, resp, err := g.automationExchange(r.Context(), r.URL.Path, body, deadline)
	if err != nil {
		writeError(w, http.StatusBadGateway)
		return
	}
	// The desktop is untrusted content. Parse and re-encode its bounded JSON;
	// never relay its headers, cookies, redirects or executable content types.
	encoded, err := json.Marshal(json.RawMessage(resp))
	if err != nil {
		writeError(w, http.StatusBadGateway)
		return
	}
	writeJSON(w, status, encoded)
}

// Separate from computeExchange: desktop commands never execute under the UI
// or compute identity. Both metadata and SO_PEERCRED are checked before bytes
// are sent, and one absolute deadline bounds the complete exchange.
func (g *gateway) automationExchange(parent context.Context, path string, body []byte, deadline time.Time) (int, []byte, error) {
	bad := errors.New("desktop automation unavailable")
	if (path != "/exec" && path != "/computer") || len(body) == 0 || len(body) > maxAutomationRequest {
		return 0, nil, bad
	}
	ctx, cancel := context.WithDeadline(parent, deadline)
	defer cancel()
	if ctx.Err() != nil {
		return 0, nil, bad
	}
	if err := checkSocketPath(g.cfg.automationSocket, g.cfg.automationUID, g.cfg.automationUID, g.cfg.automationGID); err != nil {
		return 0, nil, bad
	}
	d := net.Dialer{Timeout: g.cfg.dial}
	c, err := d.DialContext(ctx, "unix", g.cfg.automationSocket)
	if err != nil {
		return 0, nil, bad
	}
	defer c.Close()
	uc, ok := c.(*net.UnixConn)
	if !ok {
		return 0, nil, bad
	}
	_ = uc.SetDeadline(deadline)
	stop := context.AfterFunc(ctx, func() { _ = uc.SetDeadline(time.Unix(1, 0)) })
	defer stop()
	if uid, err := unixPeerUID(uc); err != nil || uid != g.cfg.automationPeerUID {
		return 0, nil, bad
	}
	head := fmt.Sprintf("POST %s HTTP/1.1\r\nHost: desktop\r\nContent-Type: application/json\r\nContent-Length: %d\r\nConnection: close\r\n\r\n", path, len(body))
	if _, err = io.Copy(uc, io.MultiReader(bytes.NewBufferString(head), bytes.NewReader(body))); err != nil {
		return 0, nil, bad
	}
	br := bufio.NewReader(uc)
	// net/http's response parser alone has no small header-size cap.
	var header []byte
	for len(header) < maxHeaderBytes {
		b, e := br.ReadByte()
		if e != nil {
			return 0, nil, bad
		}
		header = append(header, b)
		if bytes.HasSuffix(header, []byte("\r\n\r\n")) {
			break
		}
	}
	if !bytes.HasSuffix(header, []byte("\r\n\r\n")) {
		return 0, nil, bad
	}
	resp, err := http.ReadResponse(bufio.NewReader(io.MultiReader(bytes.NewReader(header), br)), &http.Request{Method: "POST"})
	if err != nil {
		return 0, nil, bad
	}
	// Closing an untrusted oversized response body can otherwise drain it.
	// Tear down the socket first, including on an early validation failure.
	defer func() { _ = uc.Close(); _ = resp.Body.Close() }()
	if (resp.StatusCode != 200 && resp.StatusCode != 400 && resp.StatusCode != 429 && resp.StatusCode != 503) ||
		resp.ContentLength < 2 || resp.ContentLength > maxAutomationResponse || len(resp.TransferEncoding) != 0 || !jsonContentType(resp.Header) || resp.Header.Get("Content-Encoding") != "" {
		return 0, nil, bad
	}
	data, err := io.ReadAll(io.LimitReader(resp.Body, maxAutomationResponse+1))
	if err != nil || int64(len(data)) != resp.ContentLength || ctx.Err() != nil {
		return 0, nil, bad
	}
	top, ok := strictObject(data, maxJSONDepth)
	if !ok {
		return 0, nil, bad
	}
	if _, ok := top["ok"].(bool); !ok {
		return 0, nil, bad
	}
	return resp.StatusCode, data, nil
}
