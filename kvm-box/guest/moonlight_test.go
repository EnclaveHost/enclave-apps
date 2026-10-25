package main

import (
	"net/http/httptest"
	"testing"
)

func TestTunnelRejectsNonSunshineDestinations(t *testing.T) {
	for _, target := range []string{"/api/moonlight/tunnel?transport=tcp&port=22", "/api/moonlight/tunnel?transport=tcp&port=47990", "/api/moonlight/tunnel?transport=udp&port=53", "/api/moonlight/tunnel?transport=unix&port=47989", "/api/moonlight/tunnel?transport=tcp&port=-1"} {
		r := httptest.NewRequest("GET", target, nil)
		r.Header.Set("Upgrade", "kvm-box-tunnel")
		w := httptest.NewRecorder()
		moonlightTunnel(w, r)
		if w.Code != 400 {
			t.Fatalf("%s got %d", target, w.Code)
		}
	}
}
func TestTunnelRequiresExplicitUpgrade(t *testing.T) {
	r := httptest.NewRequest("GET", "/api/moonlight/tunnel?transport=tcp&port=47989", nil)
	w := httptest.NewRecorder()
	moonlightTunnel(w, r)
	if w.Code != 400 {
		t.Fatal(w.Code)
	}
}
func TestPairRequiresActionHeader(t *testing.T) {
	r := httptest.NewRequest("POST", "/api/moonlight/pair", nil)
	w := httptest.NewRecorder()
	moonlightPair(w, r)
	if w.Code != 403 {
		t.Fatal(w.Code)
	}
}
