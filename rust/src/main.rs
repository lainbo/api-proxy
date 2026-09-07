//! api-proxy（Rust 版）
//!
//! 薄代理：按路径前缀把请求转发到 AI/API 上游。
//! 与 Go 版的语义对照及已知差异见 rust/README.md。

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use http_body_util::{combinators::BoxBody, BodyExt, Empty, Full};
use hyper::body::{Body, Frame, Incoming};
use hyper::header::HeaderValue;
use hyper::service::service_fn;
use hyper::{HeaderMap, Request, Response, StatusCode, Uri};
use hyper_util::client::legacy::{
    connect::Connection as LegacyConnection, connect::HttpConnector, Client,
};
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use tower_service::Service as TowerService;

type BoxError = Box<dyn std::error::Error + Send + Sync>;
type ReqBody = BoxBody<Bytes, BoxError>;
const VERSION: &str = match option_env!("API_PROXY_VERSION") {
    Some(version) => version,
    None => "dev",
};

// ── 配置 ──────────────────────────────────────────────────

fn env_int(key: &str, fallback: u64) -> u64 {
    match std::env::var(key) {
        Ok(v) => v
            .trim()
            .parse::<u64>()
            .ok()
            .filter(|n| *n > 0)
            .unwrap_or(fallback),
        Err(_) => fallback,
    }
}

fn response_header_timeout() -> Duration {
    Duration::from_millis(env_int("PROXY_TIMEOUT_MS", 300_000))
}

/// PROXY_QUIET=1 时关闭全部运行时日志；进程启动后不可变
fn quiet() -> bool {
    static QUIET: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *QUIET.get_or_init(|| std::env::var("PROXY_QUIET").as_deref() == Ok("1"))
}

// ── 上游传输抽象 ─────────────────────────────────────────
//
// 生产环境用 hyper 连接池客户端；测试可注入假实现（对应 Go 测试
// 里替换 http.RoundTripper 的做法）。

type TransportFut = Pin<Box<dyn Future<Output = Result<Response<ReqBody>, BoxError>> + Send>>;

trait UpstreamTransport: Send + Sync + 'static {
    fn send(&self, req: Request<ReqBody>) -> TransportFut;
}

/// hyper legacy client：连接池、ALPN HTTP/2、系统 CA
struct HyperTransport<C> {
    client: Client<C, ReqBody>,
}

impl<C> UpstreamTransport for HyperTransport<C>
where
    C: TowerService<Uri, Error: Into<BoxError>> + Clone + Send + Sync + 'static,
    C::Response: hyper::rt::Read + hyper::rt::Write + LegacyConnection + Unpin + Send + 'static,
    C::Future: Unpin + Send,
{
    fn send(&self, req: Request<ReqBody>) -> TransportFut {
        let fut = self.client.request(req);
        Box::pin(async move {
            let res: Response<Incoming> = fut.await?;
            Ok(res.map(|body| body.map_err(Into::into).boxed()))
        })
    }
}

/// 包装请求体：完整写完（EOS）或被传输层释放时通知一次。
/// 对应 Go ResponseHeaderTimeout 的计时起点 —— 请求写完才开始等响应头。
struct NotifyOnEnd {
    inner: ReqBody,
    done: Option<tokio::sync::oneshot::Sender<()>>,
}

impl NotifyOnEnd {
    fn notify(&mut self) {
        if let Some(tx) = self.done.take() {
            let _ = tx.send(());
        }
    }
}

impl Body for NotifyOnEnd {
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<Frame<Bytes>, BoxError>>> {
        let this = self.get_mut();
        let res = Pin::new(&mut this.inner).poll_frame(cx);
        if matches!(res, std::task::Poll::Ready(None)) {
            this.notify();
        }
        res
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> hyper::body::SizeHint {
        self.inner.size_hint()
    }
}

impl Drop for NotifyOnEnd {
    fn drop(&mut self) {
        self.notify();
    }
}

#[derive(Clone)]
struct TimeoutConnector(hyper_rustls::HttpsConnector<HttpConnector>);

impl TowerService<Uri> for TimeoutConnector {
    type Response = <hyper_rustls::HttpsConnector<HttpConnector> as TowerService<Uri>>::Response;
    type Error = BoxError;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        self.0.poll_ready(cx)
    }

    fn call(&mut self, uri: Uri) -> Self::Future {
        let connecting = self.0.call(uri);
        Box::pin(async move {
            // DNS、TCP 和 TLS 共用 40 秒上限，计时不覆盖请求体上传。
            tokio::time::timeout(Duration::from_secs(40), connecting).await?
        })
    }
}

fn build_client() -> Client<TimeoutConnector, ReqBody> {
    let _ = rustls::crypto::ring::default_provider().install_default();

    let mut http = HttpConnector::new();
    http.set_connect_timeout(Some(Duration::from_secs(30)));
    http.set_keepalive(Some(Duration::from_secs(30)));
    http.enforce_http(false); // 默认只放行 http scheme，必须关闭才能连 https 上游

    let https = hyper_rustls::HttpsConnectorBuilder::new()
        .with_native_roots()
        .expect("failed to load system root certificates")
        .https_or_http()
        .enable_all_versions() // HTTP/1.1 + HTTP/2 (ALPN)，对应 ForceAttemptHTTP2
        .wrap_connector(http);

    Client::builder(TokioExecutor::new())
        .pool_timer(TokioTimer::new())
        .pool_idle_timeout(Duration::from_secs(90)) // IdleConnTimeout
        .pool_max_idle_per_host(10) // MaxIdleConnsPerHost
        .build(TimeoutConnector(https))
}

// ── 路由 ──────────────────────────────────────────────────

struct Route {
    prefix: &'static str,
    /// 形如 "https://api.anthropic.com" 的目标
    target: &'static str,
}

/// 与 Go 版 `pathMappings` 一致
const PATH_MAPPINGS: &[Route] = &[
    Route {
        prefix: "/anthropic",
        target: "https://api.anthropic.com",
    },
    Route {
        prefix: "/gemini",
        target: "https://generativelanguage.googleapis.com",
    },
    Route {
        prefix: "/openai",
        target: "https://api.openai.com",
    },
    Route {
        prefix: "/openrouter",
        target: "https://openrouter.ai/api",
    },
    Route {
        prefix: "/perplexity",
        target: "https://api.perplexity.ai",
    },
    Route {
        prefix: "/xai",
        target: "https://api.x.ai",
    },
    Route {
        prefix: "/telegram",
        target: "https://api.telegram.org",
    },
    Route {
        prefix: "/discord",
        target: "https://discord.com/api",
    },
    Route {
        prefix: "/groq",
        target: "https://api.groq.com/openai",
    },
    Route {
        prefix: "/cohere",
        target: "https://api.cohere.ai",
    },
    Route {
        prefix: "/huggingface",
        target: "https://api-inference.huggingface.co",
    },
    Route {
        prefix: "/together",
        target: "https://api.together.xyz",
    },
    Route {
        prefix: "/novita",
        target: "https://api.novita.ai",
    },
    Route {
        prefix: "/portkey",
        target: "https://api.portkey.ai",
    },
    Route {
        prefix: "/fireworks",
        target: "https://api.fireworks.ai/inference",
    },
];

struct ParsedTarget {
    scheme: &'static str,
    authority: String,
    /// 静态 base path（转义形式），可能为空
    base_path: String,
    query: Option<String>,
}

fn parse_target(target: &str, prefix: &str) -> ParsedTarget {
    let uri = Uri::try_from(target)
        .unwrap_or_else(|e| panic!("invalid URL for {prefix}: {target:?}: {e}"));
    if uri.scheme_str() != Some("https") || uri.authority().is_none() {
        panic!("invalid URL for {prefix}: {target:?}");
    }
    let authority = uri.authority().unwrap().as_str().to_string();
    let base_path = uri.path().to_string();
    let query = uri.query().map(str::to_string);
    ParsedTarget {
        scheme: "https",
        authority,
        base_path,
        query,
    }
}

// ── 应用状态 ──────────────────────────────────────────────

struct AppState {
    routes: Vec<(String, ParsedTarget)>,
    transport: Arc<dyn UpstreamTransport>,
    header_timeout: Duration,
    start: Instant,
}

impl AppState {
    fn new() -> Arc<Self> {
        Self::with_transport(
            Arc::new(HyperTransport {
                client: build_client(),
            }),
            response_header_timeout(),
        )
    }

    fn with_transport(
        transport: Arc<dyn UpstreamTransport>,
        header_timeout: Duration,
    ) -> Arc<Self> {
        let routes = PATH_MAPPINGS
            .iter()
            .map(|r| (r.prefix.to_string(), parse_target(r.target, r.prefix)))
            .collect();
        Arc::new(Self {
            routes,
            transport,
            header_timeout,
            start: Instant::now(),
        })
    }
}

// ── URL 改写（对应 Go rewriteURL）────────────────────────

fn has_path_prefix(path: &str, prefix: &str) -> bool {
    path == prefix || path.starts_with(&format!("{prefix}/"))
}

fn join_path(base: &str, suffix: &str) -> String {
    if base.ends_with('/') && suffix.starts_with('/') {
        format!("{base}{}", &suffix[1..])
    } else if !base.ends_with('/') && !suffix.is_empty() && !suffix.starts_with('/') {
        format!("{base}/{suffix}")
    } else {
        format!("{base}{suffix}")
    }
}

fn join_query<'a>(base: Option<&'a str>, query: impl Into<Option<&'a str>>) -> Option<String> {
    match (base, query.into()) {
        (Some(b), Some(q)) => Some(format!("{b}&{q}")),
        (Some(b), None) => Some(b.to_string()),
        (None, q) => q.map(str::to_string),
    }
}

/// 计算上游 URI。入参路径为线上原始（转义）形式，逐字节保留。
fn build_upstream_uri(route_index: usize, state: &AppState, in_uri: &Uri) -> Result<Uri, String> {
    let (prefix, target) = &state.routes[route_index];
    let in_path = in_uri.path(); // hyper 返回的是线上转义形式
    let suffix = &in_path[prefix.len()..];

    let target_path = if target.base_path.is_empty() || has_path_prefix(suffix, &target.base_path) {
        suffix.to_string()
    } else {
        join_path(&target.base_path, suffix)
    };
    let target_path = if target_path.is_empty() {
        "/".to_string()
    } else {
        target_path
    };

    let query = join_query(target.query.as_deref(), in_uri.query());

    let pq = match &query {
        Some(q) => format!("{target_path}?{q}"),
        None => target_path,
    };
    Uri::try_from(format!("{}://{}{}", target.scheme, target.authority, pq))
        .map_err(|e| format!("build upstream uri: {e}"))
}

/// 只匹配完整前缀段：path == prefix 或 path 以 prefix+"/" 开头
fn match_route(state: &AppState, escaped_path: &str) -> Option<usize> {
    state
        .routes
        .iter()
        .position(|(prefix, _)| has_path_prefix(escaped_path, prefix))
}

// ── 头部处理 ─────────────────────────────────────────────

/// Go httputil.removeHopByHopHeaders + ReverseProxy 特例
const HOP_BY_HOP_HEADERS: &[&str] = &[
    "connection",
    "proxy-connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// 请求侧剥离来源/隐私头，与 Go requestHeadersToStrip 一致
const PRIVACY_HEADERS: &[&str] = &[
    "x-forwarded-for",
    "x-forwarded-host",
    "x-forwarded-proto",
    "x-forwarded-port",
    "x-real-ip",
    "forwarded",
    "via",
    "remote-host",
    "cf-connecting-ip",
    "true-client-ip",
    "x-client-ip",
    "x-cluster-client-ip",
    "fastly-client-ip",
    "x-appengine-user-ip",
    "x-azure-clientip",
    "origin",
    "referer",
];

fn connection_tokens(headers: &HeaderMap) -> Vec<String> {
    headers
        .get_all(hyper::header::CONNECTION)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(|t| t.trim().to_ascii_lowercase())
        .filter(|t| !t.is_empty())
        .collect()
}

/// Connection 里含 "upgrade" 时返回 Upgrade 字段值
fn upgrade_type(headers: &HeaderMap) -> Option<HeaderValue> {
    let wants_upgrade = connection_tokens(headers).iter().any(|t| t == "upgrade");
    if !wants_upgrade {
        return None;
    }
    headers.get(hyper::header::UPGRADE).cloned()
}

/// 请求侧头部清洗：去 hop-by-hop（含 Connection 点名的头）、隐私头；
/// host/content-length 由传输层按新 URI / 实际 body 重算。
/// Upgrade 特例与 Go 一致：Connection 含 upgrade 时保留 `Connection: Upgrade` + `Upgrade`。
fn forward_request_headers(src: &HeaderMap) -> HeaderMap {
    let nominated = connection_tokens(src);
    let upgrade = upgrade_type(src);

    let mut out = HeaderMap::with_capacity(src.len() + 2);
    for (name, value) in src {
        let n = name.as_str();
        if n == "host"
            || n == "content-length"
            || HOP_BY_HOP_HEADERS.contains(&n)
            || PRIVACY_HEADERS.contains(&n)
            || nominated.iter().any(|t| t == n)
        {
            continue;
        }
        out.append(name.clone(), value.clone());
    }
    if let Some(upgrade_value) = upgrade {
        out.insert(
            hyper::header::CONNECTION,
            HeaderValue::from_static("Upgrade"),
        );
        out.insert(hyper::header::UPGRADE, upgrade_value);
    }
    out
}

/// 响应侧头部清洗：只做必要的 hop-by-hop 清理（含 Connection 点名的头）
fn filter_response_headers(src: &HeaderMap) -> HeaderMap {
    let nominated = connection_tokens(src);
    let mut out = HeaderMap::with_capacity(src.len());
    for (name, value) in src {
        let n = name.as_str();
        if HOP_BY_HOP_HEADERS.contains(&n) || nominated.iter().any(|t| t == n) {
            continue;
        }
        out.append(name.clone(), value.clone());
    }
    out
}

// ── 日志与错误响应 ────────────────────────────────────────

fn iso_from_unix(secs: u64, millis: u32) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{millis:03}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// Howard Hinnant 的 civil_from_days：Unix 天数 → (年, 月, 日)
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn now_iso() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    iso_from_unix(now.as_secs(), now.subsec_millis())
}

#[derive(Clone)]
enum LogVal {
    Str(String),
    Int(i64),
}

fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

fn log_json(level: &str, msg: &str, extra: &[(&str, LogVal)]) {
    if quiet() {
        return;
    }
    let mut line = format!(
        "{{\"ts\":\"{}\",\"level\":\"{}\",\"msg\":\"{}\"",
        now_iso(),
        json_escape(level),
        json_escape(msg)
    );
    for (k, v) in extra {
        match v {
            LogVal::Str(s) => line.push_str(&format!(",\"{k}\":\"{}\"", json_escape(s))),
            LogVal::Int(i) => line.push_str(&format!(",\"{k}\":{i}")),
        }
    }
    line.push('}');
    if level == "error" {
        eprintln!("{line}");
    } else {
        println!("{line}");
    }
}

fn simple_response(status: StatusCode, content_type: &str, body: &str) -> Response<ReqBody> {
    let mut builder = Response::builder().status(status);
    if !content_type.is_empty() {
        builder = builder.header(hyper::header::CONTENT_TYPE, content_type);
    }
    builder
        .body(if body.is_empty() {
            Empty::<Bytes>::new()
                .map_err(|e| -> BoxError { Box::new(e) })
                .boxed()
        } else {
            Full::new(Bytes::copy_from_slice(body.as_bytes()))
                .map_err(|e| -> BoxError { Box::new(e) })
                .boxed()
        })
        .expect("valid response")
}

struct RequestMeta {
    start: Instant,
    path: String,
    route: String,
}

impl RequestMeta {
    fn log_fields(&self, error: &str) -> Vec<(&'static str, LogVal)> {
        vec![
            ("path", LogVal::Str(self.path.clone())),
            ("upstream", LogVal::Str(self.route.clone())),
            ("error", LogVal::Str(error.to_string())),
            (
                "durationMs",
                LogVal::Int(self.start.elapsed().as_millis() as i64),
            ),
        ]
    }
}

// ── 请求处理 ─────────────────────────────────────────────

async fn health_response(state: &AppState) -> Response<ReqBody> {
    let uptime = state.start.elapsed().as_secs();
    let body = format!(
        "{{\"status\":\"ok\",\"runtime\":\"rust\",\"version\":\"{}\",\"uptime\":{uptime},\"timestamp\":\"{}\"}}",
        json_escape(VERSION),
        now_iso()
    );
    simple_response(StatusCode::OK, "application/json", &body)
}

async fn handle<B>(state: Arc<AppState>, req: Request<B>) -> Response<ReqBody>
where
    B: Body<Data = Bytes> + Send + Sync + 'static,
    B::Error: Into<BoxError>,
{
    if req.uri().path() == "/health" {
        return health_response(&state).await;
    }

    let escaped_path = req.uri().path().to_string();
    let Some(route_index) = match_route(&state, &escaped_path) else {
        log_json(
            "warn",
            "No route matched",
            &[("path", LogVal::Str(escaped_path))],
        );
        return simple_response(StatusCode::NOT_FOUND, "text/plain", "Not Found");
    };

    let meta = Arc::new(RequestMeta {
        start: Instant::now(),
        path: escaped_path,
        route: state.routes[route_index].0.clone(),
    });

    let upstream_uri = match build_upstream_uri(route_index, &state, req.uri()) {
        Ok(uri) => uri,
        Err(e) => {
            log_json("error", "Bad Gateway", &meta.log_fields(&e));
            return simple_response(StatusCode::BAD_GATEWAY, "", "");
        }
    };

    let method = req.method().clone();
    let headers = forward_request_headers(req.headers());
    let (body_done_tx, body_done_rx) = tokio::sync::oneshot::channel();
    let body = NotifyOnEnd {
        inner: req.into_body().map_err(Into::into).boxed(),
        done: Some(body_done_tx),
    }
    .boxed();

    let mut upstream_req_builder = Request::builder().method(method).uri(upstream_uri);
    *upstream_req_builder
        .headers_mut()
        .expect("builder always provides headers") = headers;
    let upstream_req = upstream_req_builder
        .body(body)
        .expect("valid upstream request");

    // PROXY_TIMEOUT_MS 只约束等待上游响应头；与 Go ResponseHeaderTimeout
    // 一致，请求体写完后才开始计时，不限制慢速上传
    let header_deadline = async {
        let _ = body_done_rx.await;
        tokio::time::sleep(state.header_timeout).await;
    };
    let send_result = tokio::select! {
        res = state.transport.send(upstream_req) => Some(res),
        _ = header_deadline => None,
    };
    match send_result {
        None => {
            log_json(
                "error",
                "Upstream request timed out",
                &meta.log_fields("response header timeout"),
            );
            simple_response(StatusCode::GATEWAY_TIMEOUT, "", "")
        }
        Some(Err(e)) => {
            log_json("error", "Bad Gateway", &meta.log_fields(&e.to_string()));
            simple_response(StatusCode::BAD_GATEWAY, "", "")
        }
        Some(Ok(upstream_res)) => {
            let mut builder = Response::builder().status(upstream_res.status());
            *builder
                .headers_mut()
                .expect("builder always provides headers") =
                filter_response_headers(upstream_res.headers());

            // 上游中途断流时状态码已发出无法变更，只能记录后原样截断响应
            let body_meta = Arc::clone(&meta);
            let body = upstream_res
                .into_body()
                .map_err(move |e| {
                    log_json(
                        "error",
                        "Upstream body copy failed",
                        &body_meta.log_fields(&e.to_string()),
                    );
                    e
                })
                .boxed();
            builder.body(body).expect("valid response")
        }
    }
}

// ── 启动 ─────────────────────────────────────────────────

fn listen_host() -> String {
    match std::env::var("BIND_HOST") {
        Ok(v) if !v.trim().is_empty() => v.trim().to_string(),
        _ => "127.0.0.1".to_string(),
    }
}

fn listen_port() -> String {
    match std::env::var("PORT") {
        Ok(v) if !v.trim().is_empty() => v.trim().to_string(),
        _ => "8000".to_string(),
    }
}

/// 与 Go net.JoinHostPort 一致：含冒号的主机名加方括号
fn listen_addr() -> String {
    let host = listen_host();
    if host.contains(':') {
        format!("[{host}]:{}", listen_port())
    } else {
        format!("{host}:{}", listen_port())
    }
}

async fn shutdown_signal(tx: tokio::sync::watch::Sender<bool>) {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    log_json("info", "Shutting down gracefully", &[]);
    let _ = tx.send(true);
}

async fn serve_connection(
    state: Arc<AppState>,
    io: tokio::net::TcpStream,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
) {
    let service = service_fn(move |req: Request<Incoming>| {
        let state = state.clone();
        async move { Ok::<_, std::convert::Infallible>(handle(state, req).await) }
    });

    let conn = hyper::server::conn::http1::Builder::new()
        // header_read_timeout 依赖 timer，未设置时连接一建立就 panic
        .timer(TokioTimer::new())
        .header_read_timeout(Duration::from_secs(10)) // 对应 Go ReadHeaderTimeout
        .serve_connection(TokioIo::new(io), service);
    tokio::pin!(conn);

    tokio::select! {
        res = &mut conn => {
            if let Err(e) = res {
                // 连接级错误（客户端断开等）：安静忽略常见情况
                let msg = e.to_string();
                if !quiet() && !msg.contains("connection reset") && !msg.contains("broken pipe") {
                    eprintln!("{msg}");
                }
            }
        }
        _ = shutdown_rx.changed() => {
            conn.as_mut().graceful_shutdown();
            let _ = conn.await;
        }
    }
}

#[tokio::main]
async fn main() {
    let addr = listen_addr();
    let state = AppState::new();

    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .unwrap_or_else(|e| panic!("failed to bind {addr}: {e}"));

    let (tx, mut rx) = tokio::sync::watch::channel(false);
    tokio::spawn(shutdown_signal(tx));

    if !quiet() {
        println!("Listening on {addr}");
    }
    let mut tasks = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                match accepted {
                    Ok((io, _peer)) => {
                        let state = state.clone();
                        let rx = rx.clone();
                        tasks.spawn(serve_connection(state, io, rx));
                    }
                    Err(e) => {
                        eprintln!("accept error: {e}");
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                }
            }
            _ = tasks.join_next(), if !tasks.is_empty() => {}
            _ = rx.changed() => break,
        }
    }

    // 对应 Go Shutdown(ctx 30s)：给在途请求最多 30s 完成
    let _ = tokio::time::timeout(Duration::from_secs(30), async {
        while tasks.join_next().await.is_some() {}
    })
    .await;
}

// ── 测试 ─────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use hyper::Method;

    use std::sync::Mutex;

    // ---- 假上游传输（对应 Go 的 roundTripFunc）---------------------------

    #[derive(Clone)]
    enum MockMode {
        Respond(u16, &'static str, Vec<(&'static str, &'static str)>),
        Hang,
        Fail(&'static str),
    }

    #[derive(Default)]
    struct Captured {
        method: Option<String>,
        path: Option<String>,
        query: Option<String>,
        authority: Option<String>,
        headers: Vec<(String, String)>,
    }

    impl Captured {
        fn header(&self, name: &str) -> Option<&str> {
            self.headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.as_str())
        }
    }

    struct MockTransport {
        mode: MockMode,
        captured: Arc<Mutex<Captured>>,
    }

    impl UpstreamTransport for MockTransport {
        fn send(&self, req: Request<ReqBody>) -> TransportFut {
            {
                let mut c = self.captured.lock().unwrap();
                c.method = Some(req.method().to_string());
                c.path = Some(req.uri().path().to_string());
                c.query = Some(req.uri().query().unwrap_or("").to_string());
                c.authority = req
                    .uri()
                    .authority()
                    .map(|value| value.as_str().to_string());
                c.headers = req
                    .headers()
                    .iter()
                    .map(|(k, v)| {
                        (
                            k.as_str().to_string(),
                            String::from_utf8_lossy(v.as_bytes()).into_owned(),
                        )
                    })
                    .collect();
            }
            match self.mode.clone() {
                MockMode::Respond(status, body, headers) => Box::pin(async move {
                    let mut b = Response::builder().status(StatusCode::from_u16(status).unwrap());
                    for (k, v) in headers {
                        b = b.header(k, v);
                    }
                    Ok(b.body(
                        Full::new(Bytes::from_static(body.as_bytes()))
                            .map_err(Into::into)
                            .boxed(),
                    )
                    .unwrap())
                }),
                MockMode::Hang => Box::pin(std::future::pending()),
                MockMode::Fail(msg) => {
                    Box::pin(async move { Err::<Response<ReqBody>, _>(BoxError::from(msg)) })
                }
            }
        }
    }

    fn mock_state(mode: MockMode) -> (Arc<AppState>, Arc<Mutex<Captured>>) {
        let captured: Arc<Mutex<Captured>> = Arc::default();
        let transport = Arc::new(MockTransport {
            mode,
            captured: captured.clone(),
        });
        (
            AppState::with_transport(transport, Duration::from_millis(150)),
            captured,
        )
    }

    async fn get(state: Arc<AppState>, uri: &str, headers: &[(&str, &str)]) -> Response<ReqBody> {
        let mut builder = Request::builder().method(Method::GET).uri(uri);
        for (k, v) in headers {
            builder = builder.header(*k, *v);
        }
        let req = builder.body(Empty::<Bytes>::new().boxed()).unwrap();
        handle(state, req).await
    }

    // ---- 路由边界 --------------------------------------------------------

    #[tokio::test]
    async fn route_boundary_prevents_host_escape() {
        let (state, captured) = mock_state(MockMode::Respond(200, "unexpected", vec![]));

        let resp = get(state, "http://proxy/openai@attacker.example/collect", &[]).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        assert!(
            captured.lock().unwrap().method.is_none(),
            "request escaped the route boundary"
        );
    }

    #[tokio::test]
    async fn unknown_prefix_returns_404() {
        let (state, captured) = mock_state(MockMode::Respond(200, "", vec![]));

        let resp = get(state, "http://proxy/nope/v1/x", &[]).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        assert!(captured.lock().unwrap().method.is_none());
    }

    // ---- URL 改写 --------------------------------------------------------

    #[tokio::test]
    async fn rewrite_pins_upstream_and_preserves_escaped_path() {
        let (state, captured) = mock_state(MockMode::Respond(200, "ok", vec![]));

        let resp = get(
            state,
            "http://proxy/openai/files/a%2Fb%25c?download=1",
            &[("Authorization", "Bearer test"), ("X-Custom", "yes")],
        )
        .await;

        assert_eq!(resp.status(), StatusCode::OK);
        let body = BodyExt::collect(resp.into_body()).await.unwrap().to_bytes();
        assert_eq!(&body[..], b"ok");

        let c = captured.lock().unwrap();
        assert_eq!(c.authority.as_deref(), Some("api.openai.com"));
        assert_eq!(c.header("host"), None);
        assert_eq!(c.path.as_deref(), Some("/files/a%2Fb%25c"));
        assert_eq!(c.query.as_deref(), Some("download=1"));
        assert_eq!(c.header("Authorization"), Some("Bearer test"));
        assert_eq!(c.header("X-Custom"), Some("yes"));
    }

    #[tokio::test]
    async fn base_path_deduplication_uses_segment_boundary() {
        struct Case {
            request_path: &'static str,
            want_host: &'static str,
            want_path: &'static str,
        }
        let cases = [
            Case {
                request_path: "/openrouter/v1/models",
                want_host: "openrouter.ai",
                want_path: "/api/v1/models",
            },
            Case {
                request_path: "/openrouter/api/v1/models",
                want_host: "openrouter.ai",
                want_path: "/api/v1/models",
            },
            Case {
                request_path: "/openrouter/apiv2/models",
                want_host: "openrouter.ai",
                want_path: "/api/apiv2/models",
            },
            Case {
                request_path: "/openrouter",
                want_host: "openrouter.ai",
                want_path: "/api",
            },
            Case {
                request_path: "/groq/openai/v1/models",
                want_host: "api.groq.com",
                want_path: "/openai/v1/models",
            },
        ];

        for case in cases {
            let (state, captured) = mock_state(MockMode::Respond(200, "ok", vec![]));

            let resp = get(state, &format!("http://proxy{}", case.request_path), &[]).await;
            assert_eq!(resp.status(), StatusCode::OK, "{}", case.request_path);

            let c = captured.lock().unwrap();
            assert_eq!(
                c.authority.as_deref(),
                Some(case.want_host),
                "{}",
                case.request_path
            );
            assert_eq!(
                c.path.as_deref(),
                Some(case.want_path),
                "{}",
                case.request_path
            );
        }
    }

    // ---- 头部处理 --------------------------------------------------------

    #[tokio::test]
    async fn strips_privacy_and_hop_by_hop_headers() {
        let (state, captured) = mock_state(MockMode::Respond(
            200,
            "ok",
            vec![
                ("connection", "X-Upstream"),
                ("x-upstream", "remove me"),
                ("x-keep", "keep me"),
            ],
        ));

        let resp = get(
            state,
            "http://proxy/openai/v1/models",
            &[
                ("connection", "X-Remove, Upgrade"),
                ("x-remove", "remove me"),
                ("upgrade", "websocket"),
                ("proxy-authorization", "Basic secret"),
                ("x-forwarded-for", "203.0.113.1"),
                ("x-real-ip", "203.0.113.1"),
                ("origin", "https://private.example"),
                ("referer", "https://private.example/path"),
            ],
        )
        .await;

        // 响应侧：hop-by-hop 提名的头被移除，端到端头保留
        assert_eq!(resp.headers().get("x-upstream"), None);
        assert_eq!(
            resp.headers().get("x-keep").map(|v| v.to_str().unwrap()),
            Some("keep me")
        );

        // 请求侧：隐私头与 hop-by-hop 全部剥离
        let c = captured.lock().unwrap();
        for name in [
            "x-remove",
            "proxy-authorization",
            "x-forwarded-for",
            "x-real-ip",
            "origin",
            "referer",
        ] {
            assert_eq!(c.header(name), None, "outbound {name} should be stripped");
        }
    }

    #[test]
    fn forward_request_headers_preserves_upgrade_special_case() {
        use hyper::header::HeaderName;
        let mut headers = HeaderMap::new();
        headers.insert(
            hyper::header::CONNECTION,
            HeaderValue::from_static("X-Remove, Upgrade"),
        );
        headers.insert(
            HeaderName::from_static("x-remove"),
            HeaderValue::from_static("gone"),
        );
        headers.insert(
            hyper::header::UPGRADE,
            HeaderValue::from_static("websocket"),
        );
        headers.insert(
            hyper::header::AUTHORIZATION,
            HeaderValue::from_static("Bearer k"),
        );

        let out = forward_request_headers(&headers);
        assert_eq!(
            out.get(hyper::header::CONNECTION)
                .map(|v| v.to_str().unwrap()),
            Some("Upgrade")
        );
        assert_eq!(
            out.get(hyper::header::UPGRADE).map(|v| v.to_str().unwrap()),
            Some("websocket")
        );
        assert_eq!(
            out.get("authorization").map(|v| v.to_str().unwrap()),
            Some("Bearer k")
        );
        assert_eq!(out.get("x-remove"), None);
    }

    #[test]
    fn all_privacy_headers_are_stripped_from_requests() {
        let go_list = [
            "x-forwarded-for",
            "x-forwarded-host",
            "x-forwarded-proto",
            "x-forwarded-port",
            "x-real-ip",
            "forwarded",
            "via",
            "remote-host",
            "cf-connecting-ip",
            "true-client-ip",
            "x-client-ip",
            "x-cluster-client-ip",
            "fastly-client-ip",
            "x-appengine-user-ip",
            "x-azure-clientip",
            "origin",
            "referer",
        ];

        let mut headers = HeaderMap::new();
        for name in go_list {
            headers.insert(
                hyper::header::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_static("x"),
            );
        }
        headers.insert(
            hyper::header::HeaderName::from_static("x-end-to-end"),
            HeaderValue::from_static("keep"),
        );

        let out = forward_request_headers(&headers);
        for name in go_list {
            assert!(out.get(name).is_none(), "privacy header {name} leaked");
        }
        assert!(out.get("x-end-to-end").is_some());
    }

    // ---- 错误分类 --------------------------------------------------------

    #[tokio::test]
    async fn upstream_header_timeout_yields_504() {
        let (state, _) = mock_state(MockMode::Hang);

        let resp = get(state, "http://proxy/openai/v1/models", &[]).await;
        assert_eq!(resp.status(), StatusCode::GATEWAY_TIMEOUT);
        let body = BodyExt::collect(resp.into_body()).await.unwrap().to_bytes();
        assert!(body.is_empty(), "timeout body must be empty");
    }

    #[tokio::test]
    async fn upstream_failure_yields_502() {
        let (state, _) = mock_state(MockMode::Fail("connection refused"));

        let resp = get(state, "http://proxy/openai/v1/models", &[]).await;
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
        let body = BodyExt::collect(resp.into_body()).await.unwrap().to_bytes();
        assert!(body.is_empty(), "bad gateway body must be empty");
    }

    // ---- 健康检查 --------------------------------------------------------

    #[tokio::test]
    async fn health_returns_ok_json() {
        let (state, _) = mock_state(MockMode::Respond(200, "", vec![]));

        let resp = get(state, "http://proxy/health", &[]).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers()
                .get(hyper::header::CONTENT_TYPE)
                .map(|v| v.to_str().unwrap()),
            Some("application/json")
        );
        let body = BodyExt::collect(resp.into_body()).await.unwrap().to_bytes();
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(text.starts_with(&format!(
            "{{\"status\":\"ok\",\"runtime\":\"rust\",\"version\":\"{}\",\"uptime\":",
            json_escape(VERSION)
        )));
        assert!(text.ends_with("}"));
    }

    // ---- 纯函数 ----------------------------------------------------------

    #[test]
    fn join_path_rules_match_go() {
        assert_eq!(join_path("/api", "/v1/x"), "/api/v1/x");
        assert_eq!(join_path("/api/", "/v1"), "/api/v1");
        assert_eq!(join_path("/api", "v1"), "/api/v1");
        assert_eq!(join_path("/api", ""), "/api");
    }

    #[test]
    fn has_path_prefix_rules_match_go() {
        assert!(has_path_prefix("/api/v1", "/api"));
        assert!(has_path_prefix("/api", "/api"));
        assert!(!has_path_prefix("/apiv2", "/api"));
        assert!(!has_path_prefix("", "/api"));
    }

    #[test]
    fn iso_format_matches_go_layout() {
        assert_eq!(iso_from_unix(0, 0), "1970-01-01T00:00:00.000Z");
        assert_eq!(iso_from_unix(1_774_563_145, 42), "2026-03-26T22:12:25.042Z");
        assert_eq!(iso_from_unix(951_782_400, 0), "2000-02-29T00:00:00.000Z"); // 闰年
    }

    #[test]
    fn json_escape_matches_expectations() {
        assert_eq!(json_escape("plain"), "plain");
        assert_eq!(json_escape("say \"hi\"\\ok\n"), "say \\\"hi\\\"\\\\ok\\n");
    }

    #[test]
    fn listen_addr_defaults_to_loopback_and_supports_ipv6() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        unsafe {
            std::env::set_var("BIND_HOST", "");
            std::env::set_var("PORT", "");
        }
        assert_eq!(listen_addr(), "127.0.0.1:8000");

        unsafe {
            std::env::set_var("BIND_HOST", "::1");
            std::env::set_var("PORT", "49417");
        }
        assert_eq!(listen_addr(), "[::1]:49417");

        unsafe {
            std::env::remove_var("BIND_HOST");
            std::env::remove_var("PORT");
        }
    }

    static ENV_LOCK: Mutex<()> = Mutex::new(());
}
