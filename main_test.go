package main

import (
	"context"
	"errors"
	"io"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
)

type roundTripFunc func(*http.Request) (*http.Response, error)

func (fn roundTripFunc) RoundTrip(req *http.Request) (*http.Response, error) {
	return fn(req)
}

func withRouteTransport(t *testing.T, prefix string, transport http.RoundTripper) {
	t.Helper()
	rt := matchRoute(prefix)
	if rt == nil {
		t.Fatalf("route %s not found", prefix)
	}
	previous := rt.proxy.Transport
	rt.proxy.Transport = transport
	t.Cleanup(func() {
		rt.proxy.Transport = previous
	})
}

func upstreamResponse(status int, body string, headers http.Header) *http.Response {
	return &http.Response{
		StatusCode:    status,
		Header:        headers,
		Body:          io.NopCloser(strings.NewReader(body)),
		ContentLength: int64(len(body)),
	}
}

func TestRouteBoundaryPreventsHostEscape(t *testing.T) {
	called := false
	withRouteTransport(t, "/openai", roundTripFunc(func(*http.Request) (*http.Response, error) {
		called = true
		return upstreamResponse(http.StatusOK, "unexpected", nil), nil
	}))

	recorder := httptest.NewRecorder()
	handler(recorder, httptest.NewRequest(http.MethodGet, "http://proxy/openai@attacker.example/collect", nil))

	if recorder.Code != http.StatusNotFound {
		t.Fatalf("status = %d, want %d", recorder.Code, http.StatusNotFound)
	}
	if called {
		t.Fatal("request escaped the route boundary")
	}
}

func TestRewritePinsUpstreamAndPreservesEscapedPath(t *testing.T) {
	withRouteTransport(t, "/openai", roundTripFunc(func(req *http.Request) (*http.Response, error) {
		if req.URL.Scheme != "https" || req.URL.Host != "api.openai.com" {
			t.Fatalf("upstream URL = %s", req.URL.String())
		}
		if req.Host != "api.openai.com" {
			t.Fatalf("Host = %q", req.Host)
		}
		if got := req.URL.EscapedPath(); got != "/files/a%2Fb%25c" {
			t.Fatalf("escaped path = %q", got)
		}
		if got := req.Header.Get("Authorization"); got != "Bearer test" {
			t.Fatalf("Authorization = %q", got)
		}
		if req.URL.RawQuery != "download=1" {
			t.Fatalf("raw query = %q", req.URL.RawQuery)
		}
		if _, ok := req.Context().Deadline(); ok {
			t.Fatal("outbound request has a whole-response deadline")
		}
		return upstreamResponse(http.StatusOK, "ok", nil), nil
	}))

	req := httptest.NewRequest(http.MethodGet, "http://proxy/openai/files/a%2Fb%25c?download=1", nil)
	req.Header.Set("Authorization", "Bearer test")
	recorder := httptest.NewRecorder()
	handler(recorder, req)

	if recorder.Code != http.StatusOK || recorder.Body.String() != "ok" {
		t.Fatalf("response = %d %q", recorder.Code, recorder.Body.String())
	}
}

func TestBasePathDeduplicationUsesSegmentBoundary(t *testing.T) {
	tests := []struct {
		requestPath string
		wantPath    string
	}{
		{requestPath: "/openrouter/v1/models", wantPath: "/api/v1/models"},
		{requestPath: "/openrouter/api/v1/models", wantPath: "/api/v1/models"},
		{requestPath: "/openrouter/apiv2/models", wantPath: "/api/apiv2/models"},
	}

	for _, test := range tests {
		t.Run(test.requestPath, func(t *testing.T) {
			withRouteTransport(t, "/openrouter", roundTripFunc(func(req *http.Request) (*http.Response, error) {
				if req.URL.Host != "openrouter.ai" || req.URL.Path != test.wantPath {
					t.Fatalf("upstream URL = %s, want path %s", req.URL.String(), test.wantPath)
				}
				return upstreamResponse(http.StatusOK, "ok", nil), nil
			}))

			recorder := httptest.NewRecorder()
			handler(recorder, httptest.NewRequest(http.MethodGet, "http://proxy"+test.requestPath, nil))
			if recorder.Code != http.StatusOK {
				t.Fatalf("status = %d", recorder.Code)
			}
		})
	}
}

func TestProxyStripsPrivacyAndHopByHopHeaders(t *testing.T) {
	withRouteTransport(t, "/openai", roundTripFunc(func(req *http.Request) (*http.Response, error) {
		for _, name := range []string{
			"X-Remove", "Proxy-Authorization",
			"X-Forwarded-For", "X-Real-Ip", "Origin", "Referer",
		} {
			if value := req.Header.Get(name); value != "" {
				t.Errorf("outbound %s = %q", name, value)
			}
		}
		if req.Header.Get("Connection") != "Upgrade" || req.Header.Get("Upgrade") != "websocket" {
			t.Errorf("protocol upgrade headers were not preserved correctly")
		}
		return upstreamResponse(http.StatusOK, "ok", http.Header{
			"Connection": {"X-Upstream"},
			"X-Upstream": {"remove me"},
			"X-Keep":     {"keep me"},
		}), nil
	}))

	req := httptest.NewRequest(http.MethodGet, "http://proxy/openai/v1/models", nil)
	req.Header.Set("Connection", "X-Remove, Upgrade")
	req.Header.Set("X-Remove", "remove me")
	req.Header.Set("Upgrade", "websocket")
	req.Header.Set("Proxy-Authorization", "Basic secret")
	req.Header.Set("X-Forwarded-For", "203.0.113.1")
	req.Header.Set("X-Real-IP", "203.0.113.1")
	req.Header.Set("Origin", "https://private.example")
	req.Header.Set("Referer", "https://private.example/path")

	recorder := httptest.NewRecorder()
	handler(recorder, req)

	if got := recorder.Header().Get("X-Upstream"); got != "" {
		t.Fatalf("hop-by-hop nominated response header leaked: %q", got)
	}
	if got := recorder.Header().Get("X-Keep"); got != "keep me" {
		t.Fatalf("end-to-end response header = %q", got)
	}
}

func TestProxyErrorClassification(t *testing.T) {
	tests := []struct {
		name       string
		err        error
		wantStatus int
	}{
		{name: "timeout", err: context.DeadlineExceeded, wantStatus: http.StatusGatewayTimeout},
		{name: "upstream failure", err: errors.New("connection refused"), wantStatus: http.StatusBadGateway},
	}

	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			withRouteTransport(t, "/openai", roundTripFunc(func(*http.Request) (*http.Response, error) {
				return nil, test.err
			}))

			recorder := httptest.NewRecorder()
			handler(recorder, httptest.NewRequest(http.MethodGet, "http://proxy/openai/v1/models", nil))
			if recorder.Code != test.wantStatus {
				t.Fatalf("status = %d, want %d", recorder.Code, test.wantStatus)
			}
		})
	}
}

func TestListenAddrDefaultsToLoopback(t *testing.T) {
	t.Setenv("BIND_HOST", "")
	t.Setenv("PORT", "")
	if got := listenAddr(); got != "127.0.0.1:8000" {
		t.Fatalf("listen address = %q", got)
	}
}

func TestListenAddrSupportsExplicitIPv6(t *testing.T) {
	t.Setenv("BIND_HOST", "::1")
	t.Setenv("PORT", "49417")
	if got := listenAddr(); got != "[::1]:49417" {
		t.Fatalf("listen address = %q", got)
	}
}
