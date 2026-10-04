package main

import (
	"context"
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
	"strconv"
	"strings"
	"sync"
	"syscall"
	"time"
)

// ── 配置 ──────────────────────────────────────────────────

var (
	responseHeaderTimeout = time.Duration(envInt("PROXY_TIMEOUT_MS", 300000)) * time.Millisecond
	version               = "dev"
	quiet                 = os.Getenv("PROXY_QUIET") == "1"
)

func envInt(key string, fallback int) int {
	if v := os.Getenv(key); v != "" {
		if n, err := strconv.Atoi(v); err == nil && n > 0 {
			return n
		}
	}
	return fallback
}

// ── HTTP 传输 ─────────────────────────────────────────────

var proxyTransport = &http.Transport{
	DialContext: (&net.Dialer{
		Timeout:   30 * time.Second,
		KeepAlive: 30 * time.Second,
	}).DialContext,
	ForceAttemptHTTP2:     true,
	MaxIdleConns:          100,
	MaxIdleConnsPerHost:   10,
	IdleConnTimeout:       90 * time.Second,
	TLSHandshakeTimeout:   10 * time.Second,
	ResponseHeaderTimeout: responseHeaderTimeout,
	ExpectContinueTimeout: time.Second,
	DisableCompression:    true,
}

type proxyBufferPool struct {
	pool sync.Pool
}

func (p *proxyBufferPool) Get() []byte {
	if buf, ok := p.pool.Get().([]byte); ok {
		return buf
	}
	return make([]byte, 32*1024)
}

func (p *proxyBufferPool) Put(buf []byte) {
	p.pool.Put(buf)
}

var responseBuffers = &proxyBufferPool{}

// ── 路由 ──────────────────────────────────────────────────

type route struct {
	prefix string
	proxy  *httputil.ReverseProxy
}

var pathMappings = [][2]string{
	{"/anthropic", "https://api.anthropic.com"},
	{"/gemini", "https://generativelanguage.googleapis.com"},
	{"/openai", "https://api.openai.com"},
	{"/openai-auth", "https://auth.openai.com"},
	{"/chatgpt", "https://chatgpt.com/backend-api"},
	{"/openrouter", "https://openrouter.ai/api"},
	{"/perplexity", "https://api.perplexity.ai"},
	{"/xai", "https://api.x.ai"},
	{"/telegram", "https://api.telegram.org"},
	{"/discord", "https://discord.com/api"},
	{"/groq", "https://api.groq.com/openai"},
	{"/cohere", "https://api.cohere.ai"},
	{"/huggingface", "https://router.huggingface.co"},
	{"/together", "https://api.together.xyz"},
	{"/novita", "https://api.novita.ai"},
	{"/portkey", "https://api.portkey.ai"},
	{"/fireworks", "https://api.fireworks.ai/inference"},
	{"/bitwarden/api", "https://api.bitwarden.com"},
	{"/bitwarden/identity", "https://identity.bitwarden.com"},
	{"/bitwarden/notifications", "https://notifications.bitwarden.com"},
	{"/bitwarden/icons", "https://icons.bitwarden.net"},
	{"/bitwarden/events", "https://events.bitwarden.com"},
}

// Bitwarden 桌面端和 Safari 扩展的请求需要通过官方服务端按 Origin 做的 CORS 预检
func keepsOrigin(prefix string) bool {
	return strings.HasPrefix(prefix, "/bitwarden/")
}

var routes []route

func init() {
	for _, mapping := range pathMappings {
		prefix := mapping[0]
		target, err := url.Parse(mapping[1])
		// rewriteURL 直接拼接 base path 并原样使用入站查询参数，上游地址不能以 / 结尾或自带查询参数
		if err != nil || target.Scheme != "https" || target.Host == "" ||
			strings.HasSuffix(target.Path, "/") || target.RawQuery != "" {
			panic(fmt.Sprintf("invalid URL for %s: %q", prefix, mapping[1]))
		}
		routes = append(routes, route{prefix: prefix, proxy: newReverseProxy(prefix, target)})
	}
}

func newReverseProxy(prefix string, target *url.URL) *httputil.ReverseProxy {
	logDest := io.Writer(os.Stderr)
	if quiet {
		logDest = io.Discard
	}
	keepOrigin := keepsOrigin(prefix)
	return &httputil.ReverseProxy{
		Transport:  proxyTransport,
		BufferPool: responseBuffers,
		ErrorLog:   log.New(logDest, "", 0),
		Rewrite: func(proxyReq *httputil.ProxyRequest) {
			rewriteURL(proxyReq.Out.URL, proxyReq.In.URL, prefix, target)
			proxyReq.Out.Host = target.Host
			stripPrivacyHeaders(proxyReq.Out.Header, keepOrigin)
		},
		ErrorHandler: proxyErrorHandler,
	}
}

func matchRoute(escapedPath string) *route {
	for i := range routes {
		if hasPathPrefix(escapedPath, routes[i].prefix) {
			return &routes[i]
		}
	}
	return nil
}

func rewriteURL(out, in *url.URL, prefix string, target *url.URL) {
	suffixPath := in.Path[len(prefix):]
	suffixEscapedPath := in.EscapedPath()[len(prefix):]

	// 客户端已带上游 base path 时不再重复追加；按转义路径判断，与路由匹配的路径段边界一致
	basePath, baseEscapedPath := target.Path, target.EscapedPath()
	if hasPathPrefix(suffixEscapedPath, baseEscapedPath) {
		basePath, baseEscapedPath = "", ""
	}
	targetPath := basePath + suffixPath
	targetEscapedPath := baseEscapedPath + suffixEscapedPath
	if targetPath == "" {
		targetPath, targetEscapedPath = "/", "/"
	}

	out.Scheme = target.Scheme
	out.Host = target.Host
	out.User = nil
	out.Path = targetPath
	out.RawPath = rawPath(targetPath, targetEscapedPath)
	// ReverseProxy 会在 Rewrite 前删除无法解析的参数并重新编码，这里恢复入站的原始查询字符串
	out.RawQuery = in.RawQuery
	out.Fragment = ""
}

func hasPathPrefix(path, prefix string) bool {
	return path == prefix || strings.HasPrefix(path, prefix+"/")
}

func rawPath(path, escapedPath string) string {
	if path == escapedPath {
		return ""
	}
	return escapedPath
}

// ── 头部处理 ─────────────────────────────────────────────

var requestHeadersToStrip = []string{
	"X-Forwarded-For", "X-Forwarded-Host", "X-Forwarded-Proto",
	"X-Forwarded-Port", "X-Real-Ip", "Forwarded", "Via",
	"Remote-Host",
	"Cf-Connecting-Ip", "True-Client-Ip", "X-Client-Ip",
	"X-Cluster-Client-Ip", "Fastly-Client-Ip",
	"X-Appengine-User-Ip", "X-Azure-Clientip",
	"Origin", "Referer",
}

func stripPrivacyHeaders(headers http.Header, keepOrigin bool) {
	for _, name := range requestHeadersToStrip {
		if keepOrigin && name == "Origin" {
			continue
		}
		headers.Del(name)
	}
}

// ── 日志与错误响应 ────────────────────────────────────────

var startTime = time.Now()

type requestMeta struct {
	start time.Time
	route string
}

type requestMetaKey struct{}

func nowISO() string {
	return time.Now().UTC().Format("2006-01-02T15:04:05.000Z")
}

func logJSON(level, msg string, extra map[string]any) {
	if quiet {
		return
	}
	entry := make(map[string]any, 3+len(extra))
	entry["ts"] = nowISO()
	entry["level"] = level
	entry["msg"] = msg
	for k, v := range extra {
		entry[k] = v
	}
	data, _ := json.Marshal(entry)
	if level == "error" {
		fmt.Fprintln(os.Stderr, string(data))
		return
	}
	fmt.Fprintln(os.Stdout, string(data))
}

func classifyError(err error) (status int, msg string) {
	var netErr net.Error
	if errors.As(err, &netErr) && netErr.Timeout() {
		return http.StatusGatewayTimeout, "Upstream request timed out"
	}
	return http.StatusBadGateway, "Bad Gateway"
}

func proxyErrorHandler(w http.ResponseWriter, req *http.Request, err error) {
	meta, _ := req.Context().Value(requestMetaKey{}).(requestMeta)
	if errors.Is(req.Context().Err(), context.Canceled) {
		logJSON("warn", "Client disconnected", errorLogFields(meta, err))
		return
	}

	status, msg := classifyError(err)
	logJSON("error", msg, errorLogFields(meta, err))
	writeSimpleResponse(w, status, "", "")
}

func errorLogFields(meta requestMeta, err error) map[string]any {
	return map[string]any{
		"upstream":   meta.route,
		"error":      err.Error(),
		"durationMs": time.Since(meta.start).Milliseconds(),
	}
}

func writeSimpleResponse(w http.ResponseWriter, status int, contentType, body string) {
	if contentType != "" {
		w.Header().Set("Content-Type", contentType)
	}
	w.WriteHeader(status)
	if body != "" {
		_, _ = w.Write([]byte(body))
	}
}

// ── 请求处理 ─────────────────────────────────────────────

type healthResponse struct {
	Status    string `json:"status"`
	Version   string `json:"version"`
	Uptime    int    `json:"uptime"`
	Timestamp string `json:"timestamp"`
}

func handler(w http.ResponseWriter, req *http.Request) {
	if req.URL.Path == "/health" {
		body, _ := json.Marshal(healthResponse{
			Status:    "ok",
			Version:   version,
			Uptime:    int(time.Since(startTime).Seconds()),
			Timestamp: nowISO(),
		})
		writeSimpleResponse(w, http.StatusOK, "application/json", string(body))
		return
	}

	rt := matchRoute(req.URL.EscapedPath())
	if rt == nil {
		logJSON("warn", "No route matched", nil)
		writeSimpleResponse(w, http.StatusNotFound, "text/plain", "Not Found")
		return
	}

	meta := requestMeta{
		start: time.Now(),
		route: rt.prefix,
	}
	req = req.WithContext(context.WithValue(req.Context(), requestMetaKey{}, meta))
	rt.proxy.ServeHTTP(w, req)
}

// ── 启动 ─────────────────────────────────────────────────

func main() {
	addr := listenAddr()

	srv := &http.Server{
		Addr:              addr,
		Handler:           http.HandlerFunc(handler),
		ReadHeaderTimeout: 10 * time.Second,
	}

	// Shutdown 一被调用 ListenAndServe 就会立即返回，必须等它排空在途请求
	shutdownDone := make(chan struct{})
	go func() {
		sigCh := make(chan os.Signal, 1)
		signal.Notify(sigCh, syscall.SIGINT, syscall.SIGTERM)
		<-sigCh
		ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
		defer cancel()
		_ = srv.Shutdown(ctx)
		close(shutdownDone)
	}()

	if !quiet {
		fmt.Printf("Listening on %s\n", addr)
	}
	if err := srv.ListenAndServe(); err != nil && !errors.Is(err, http.ErrServerClosed) {
		fmt.Fprintf(os.Stderr, "Server error: %v\n", err)
		os.Exit(1)
	}
	<-shutdownDone
}

func listenAddr() string {
	host := os.Getenv("BIND_HOST")
	if host == "" {
		host = "127.0.0.1"
	}
	port := os.Getenv("PORT")
	if port == "" {
		port = "8000"
	}
	return net.JoinHostPort(host, port)
}
