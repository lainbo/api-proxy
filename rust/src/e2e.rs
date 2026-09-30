//! 本地端到端链路：HTTP 客户端 → 生产代理连接处理 → TLS 上游（同时支持 HTTP/2 和 HTTP/1.1）。
//! 验证失败场景：路由缺失或越界、转义丢失、Origin/CORS 错误、隐私头泄漏、
//! 升级误用 HTTP/2、101 握手不匹配、握手拒绝或超时、提前到达的数据丢失、
//! 大帧/控制帧损坏、长连接被响应头超时截断、双向关闭及流式响应回归。

use super::*;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::task::{Context, Poll};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, watch};

fn websocket_frame(opcode: u8, payload: &[u8], masked: bool) -> Vec<u8> {
    let mask = if masked { 0x80 } else { 0 };
    let mut frame = vec![opcode];
    match payload.len() {
        n if n < 126 => frame.push(mask | n as u8),
        n if n <= u16::MAX as usize => {
            frame.push(mask | 126);
            frame.extend_from_slice(&(n as u16).to_be_bytes());
        }
        n => {
            frame.push(mask | 127);
            frame.extend_from_slice(&(n as u64).to_be_bytes());
        }
    }
    if masked {
        frame.extend_from_slice(&[0; 4]);
    }
    frame.extend_from_slice(payload);
    frame
}

fn websocket_messages() -> Vec<(u8, Vec<u8>)> {
    vec![
        (0x81, b"early".to_vec()),
        (0x82, (0..131_072).map(|i| (i % 256) as u8).collect()),
        (0x01, b"fragment one".to_vec()),
        (0x80, b"fragment two".to_vec()),
        (0x89, b"ping".to_vec()),
        (0x88, vec![3, 232]),
    ]
}

async fn upstream_tunnel(upgrade: hyper::upgrade::OnUpgrade, path: &str) -> Result<(), BoxError> {
    let mut io = TokioIo::new(upgrade.await?);
    match path {
        "/hub" => {
            io.write_all(&websocket_frame(0x81, b"ready", false))
                .await?;
            io.flush().await?;
            for (opcode, payload) in websocket_messages() {
                let expected = websocket_frame(opcode, &payload, true);
                let mut received = vec![0; expected.len()];
                io.read_exact(&mut received).await?;
                if received != expected {
                    return Err("WebSocket frame changed in transit".into());
                }
                let opcode = if opcode == 0x89 { 0x8a } else { opcode };
                io.write_all(&websocket_frame(opcode, &payload, false))
                    .await?;
                io.flush().await?;
            }
        }
        "/half-close" => {
            let mut received = Vec::new();
            io.read_to_end(&mut received).await?;
            if received != b"last client bytes" {
                return Err("client half-close lost data".into());
            }
            io.write_all(b"last upstream bytes").await?;
        }
        "/disconnect" => {
            let mut received = Vec::new();
            io.read_to_end(&mut received).await?;
        }
        "/upstream-close" => io.write_all(b"upstream goodbye").await?,
        _ => unreachable!(),
    }
    io.shutdown().await?;
    Ok(())
}

struct StreamBody(mpsc::Receiver<Frame<Bytes>>);

impl Body for StreamBody {
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, BoxError>>> {
        self.get_mut().0.poll_recv(cx).map(|frame| frame.map(Ok))
    }
}

async fn upstream(
    mut req: Request<Incoming>,
    completed: mpsc::Sender<Result<(), String>>,
) -> Result<Response<ReqBody>, Infallible> {
    let path = req.uri().path().to_string();
    let mut response = simple_response(StatusCode::OK, "", "ok");
    for name in [
        "origin",
        "authorization",
        "referer",
        "x-forwarded-for",
        "x-remove",
    ] {
        if let Some(value) = req.headers().get(name) {
            response.headers_mut().insert(
                hyper::header::HeaderName::from_bytes(format!("x-seen-{name}").as_bytes()).unwrap(),
                value.clone(),
            );
        }
    }
    response.headers_mut().insert(
        "x-seen-uri",
        HeaderValue::from_str(req.uri().path_and_query().unwrap().as_str()).unwrap(),
    );
    response.headers_mut().insert(
        "x-seen-version",
        HeaderValue::from_str(&format!("{:?}", req.version())).unwrap(),
    );
    if req.method() == hyper::Method::OPTIONS {
        *response.status_mut() = StatusCode::NO_CONTENT;
        *response.body_mut() = Empty::<Bytes>::new().map_err(Into::into).boxed();
        if let Some(origin) = req.headers().get("origin") {
            response
                .headers_mut()
                .insert("access-control-allow-origin", origin.clone());
        }
        response.headers_mut().insert(
            "access-control-allow-methods",
            HeaderValue::from_static("POST"),
        );
    } else if path == "/hang" {
        tokio::time::sleep(Duration::from_secs(1)).await;
    } else if path == "/stream" {
        let (tx, rx) = mpsc::channel(2);
        *response.body_mut() = StreamBody(rx).boxed();
        tokio::spawn(async move {
            tx.send(Frame::data(Bytes::from_static(b"first")))
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(300)).await;
            tx.send(Frame::data(Bytes::from_static(b"second")))
                .await
                .unwrap();
        });
    } else if path == "/reject" {
        *response.status_mut() = StatusCode::UNAUTHORIZED;
        *response.body_mut() = Full::new(Bytes::from_static(b"denied"))
            .map_err(Into::into)
            .boxed();
        response
            .headers_mut()
            .insert("www-authenticate", HeaderValue::from_static("Bearer"));
    } else if matches!(
        path.as_str(),
        "/mismatch"
            | "/unsolicited"
            | "/missing-upgrade"
            | "/hub"
            | "/disconnect"
            | "/half-close"
            | "/upstream-close"
    ) {
        if path == "/unsolicited" {
            let result = if req.version() == hyper::Version::HTTP_11
                && upgrade_type(req.headers()).is_none()
            {
                Ok(())
            } else {
                Err("unsolicited 101 requires HTTP/1.1 without an upgrade request".to_string())
            };
            completed.send(result).await.unwrap();
        }
        *response.status_mut() = StatusCode::SWITCHING_PROTOCOLS;
        *response.body_mut() = Empty::<Bytes>::new().map_err(Into::into).boxed();
        response
            .headers_mut()
            .insert("connection", HeaderValue::from_static("Upgrade, X-Remove"));
        response
            .headers_mut()
            .insert("x-remove", HeaderValue::from_static("private"));
        if path != "/missing-upgrade" {
            response.headers_mut().insert(
                "upgrade",
                HeaderValue::from_static(if path == "/mismatch" {
                    "other"
                } else {
                    "WebSocket"
                }),
            );
        }
        response.headers_mut().insert(
            "sec-websocket-accept",
            HeaderValue::from_static("s3pPLMBiTxaQ9kYGzzhZRbK+xOo="),
        );
        response
            .headers_mut()
            .insert("sec-websocket-protocol", HeaderValue::from_static("e2e"));
        if matches!(
            path.as_str(),
            "/hub" | "/disconnect" | "/half-close" | "/upstream-close"
        ) {
            let upgrade = hyper::upgrade::on(&mut req);
            tokio::spawn(async move {
                let result = upstream_tunnel(upgrade, &path)
                    .await
                    .map_err(|e| e.to_string());
                let _ = completed.send(result).await;
            });
        }
    } else {
        let body = req.into_body().collect().await.unwrap().to_bytes();
        if !body.is_empty() {
            *response.body_mut() = Full::new(body).map_err(Into::into).boxed();
        }
    }
    Ok(response)
}

async fn start_upstream(
    completed: mpsc::Sender<Result<(), String>>,
    http1_only: bool,
) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let cert =
        std::fs::read(std::env::var("API_PROXY_E2E_CERT").expect("run tests/bitwarden-e2e.sh"))
            .unwrap();
    let key = std::fs::read(std::env::var("API_PROXY_E2E_KEY").unwrap()).unwrap();
    let mut tls = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(
            vec![cert.into()],
            rustls::pki_types::PrivatePkcs8KeyDer::from(key).into(),
        )
        .unwrap();
    tls.alpn_protocols = if http1_only {
        vec![b"http/1.1".to_vec()]
    } else {
        vec![b"h2".to_vec(), b"http/1.1".to_vec()]
    };
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(tls));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let mut connections = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                accepted = listener.accept() => {
                    let (io, _) = accepted.unwrap();
                    let acceptor = acceptor.clone();
                    let completed = completed.clone();
                    connections.spawn(async move {
                        let io = acceptor.accept(io).await.unwrap();
                        let h2 = io.get_ref().1.alpn_protocol() == Some(b"h2");
                        let service = service_fn(move |req| upstream(req, completed.clone()));
                        if h2 {
                            let _ = hyper::server::conn::http2::Builder::new(TokioExecutor::new())
                                .serve_connection(TokioIo::new(io), service).await;
                        } else {
                            let _ = hyper::server::conn::http1::Builder::new()
                                .serve_connection(TokioIo::new(io), service).with_upgrades().await;
                        }
                    });
                }
                result = connections.join_next(), if !connections.is_empty() => { result.unwrap().unwrap(); }
            }
        }
    });
    (address, task)
}

async fn start_proxy(
    state: Arc<AppState>,
) -> (SocketAddr, watch::Sender<bool>, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (shutdown, mut rx) = watch::channel(false);
    let task = tokio::spawn(async move {
        let mut connections = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                accepted = listener.accept() => {
                    let (io, _) = accepted.unwrap();
                    connections.spawn(serve_connection(state.clone(), io, rx.clone()));
                }
                result = connections.join_next(), if !connections.is_empty() => { result.unwrap().unwrap(); }
                _ = rx.changed() => break,
            }
        }
        while let Some(result) = connections.join_next().await {
            result.unwrap();
        }
    });
    (address, shutdown, task)
}

async fn request(
    address: SocketAddr,
    method: &str,
    path: &str,
    upgrade: bool,
) -> Response<Incoming> {
    let client = Client::builder(TokioExecutor::new()).build_http::<ReqBody>();
    let mut req = Request::builder()
        .method(method)
        .uri(format!("http://{address}{path}"))
        .header("origin", "https://vault.bitwarden.com")
        .header("authorization", "Bearer e2e-fixture")
        .header("referer", "https://private.example/")
        .header("x-forwarded-for", "192.0.2.1")
        .header("x-remove", "private")
        .header(
            "connection",
            if upgrade {
                "X-Remove, Upgrade"
            } else {
                "X-Remove"
            },
        );
    if upgrade {
        req = req.header("upgrade", "websocket");
    }
    let body = Full::new(Bytes::from_static(if method == "POST" {
        b"request-body"
    } else {
        b""
    }))
    .map_err(Into::into)
    .boxed();
    client.request(req.body(body).unwrap()).await.unwrap()
}

async fn websocket(address: SocketAddr, path: &str, early: &[u8]) -> BufReader<TcpStream> {
    let mut io = TcpStream::connect(address).await.unwrap();
    let mut request = format!("GET /bitwarden/notifications{path} HTTP/1.1\r\nHost: {address}\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Protocol: e2e\r\nOrigin: https://vault.bitwarden.com\r\nAuthorization: Bearer e2e-fixture\r\n\r\n").into_bytes();
    request.extend_from_slice(early);
    io.write_all(&request).await.unwrap();
    let mut reader = BufReader::new(io);
    let mut headers = String::new();
    loop {
        let mut line = String::new();
        assert!(
            reader.read_line(&mut line).await.unwrap() > 0,
            "missing upgrade response"
        );
        headers.push_str(&line);
        if line == "\r\n" {
            break;
        }
    }
    assert!(
        headers.contains("sec-websocket-accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo=\r\n"),
        "{headers}"
    );
    let headers = headers.to_ascii_lowercase();
    assert!(headers.starts_with("http/1.1 101"), "{headers}");
    for header in [
        "connection: upgrade\r\n",
        "upgrade: websocket\r\n",
        "sec-websocket-protocol: e2e\r\n",
        "x-seen-version: http/1.1\r\n",
        "x-seen-origin: https://vault.bitwarden.com\r\n",
        "x-seen-authorization: bearer e2e-fixture\r\n",
    ] {
        assert!(headers.contains(header), "missing {header}: {headers}");
    }
    assert!(!headers.contains("\r\nx-remove:"));
    reader
}

async fn expect_bytes(reader: &mut BufReader<TcpStream>, expected: &[u8]) {
    let mut received = vec![0; expected.len()];
    reader.read_exact(&mut received).await.unwrap();
    assert_eq!(received, expected);
}

#[tokio::test]
#[ignore = "run bash tests/bitwarden-e2e.sh to provision the local TLS fixture"]
async fn bitwarden_proxy() {
    tokio::time::timeout(Duration::from_secs(20), async {
        let (completed_tx, mut completed_rx) = mpsc::channel(8);
        let (upstream_address, upstream_task) = start_upstream(completed_tx.clone(), false).await;
        let (http1_address, http1_task) = start_upstream(completed_tx, true).await;
        let mut state = AppState::new();
        let state_mut = Arc::get_mut(&mut state).unwrap();
        state_mut.header_timeout = Duration::from_millis(150);
        let routes = [
            ("api", "api.bitwarden.com"),
            ("identity", "identity.bitwarden.com"),
            ("notifications", "notifications.bitwarden.com"),
            ("icons", "icons.bitwarden.net"),
            ("events", "events.bitwarden.com"),
        ];
        for (suffix, host) in routes {
            assert!(state_mut.routes.iter().any(|(prefix, target)| prefix == &format!("/bitwarden/{suffix}") && target.authority == host));
        }
        for (_, target) in &mut state_mut.routes {
            target.authority = upstream_address.to_string();
        }
        state_mut.routes.push(("/e2e-http1".to_string(), ParsedTarget {
            scheme: "https",
            authority: http1_address.to_string(),
            base_path: String::new(),
            query: None,
        }));
        let (proxy, shutdown, proxy_task) = start_proxy(state).await;
        let mut passed = Vec::new();

        for (suffix, _) in routes {
            for tail in ["", "/", "/files/a%2Fb%25c?x=1%2B2&x=3"] {
                let response = request(proxy, "GET", &format!("/bitwarden/{suffix}{tail}"), false).await;
                assert_eq!(response.status(), StatusCode::OK);
                assert_eq!(response.headers()["x-seen-uri"], if tail.is_empty() { "/" } else { tail });
                assert_eq!(response.headers()["x-seen-origin"], "https://vault.bitwarden.com");
                assert_eq!(response.headers()["x-seen-authorization"], "Bearer e2e-fixture");
                assert_eq!(response.headers()["x-seen-version"], "HTTP/2.0");
                for header in ["x-seen-referer", "x-seen-x-forwarded-for", "x-seen-x-remove"] {
                    assert!(!response.headers().contains_key(header));
                }
                response.into_body().collect().await.unwrap();
            }
            let response = request(proxy, "GET", &format!("/bitwarden/{suffix}-other"), false).await;
            assert_eq!(response.status(), StatusCode::NOT_FOUND);
        }
        passed.push("five_routes_path_encoding_headers_tls_http2");

        let response = request(proxy, "OPTIONS", "/bitwarden/identity/connect/token", false).await;
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert_eq!(response.headers()["access-control-allow-origin"], "https://vault.bitwarden.com");
        assert_eq!(response.headers()["access-control-allow-methods"], "POST");
        let response = request(proxy, "POST", "/bitwarden/identity/connect/token", false).await;
        assert_eq!(response.into_body().collect().await.unwrap().to_bytes(), "request-body");
        passed.push("cors_preflight_and_request_body");

        let response = request(proxy, "GET", "/openrouter/api/v1/models", false).await;
        assert_eq!(response.headers()["x-seen-uri"], "/api/v1/models");
        assert!(!response.headers().contains_key("x-seen-origin"));
        let response = request(proxy, "GET", "/openai/stream", false).await;
        assert_eq!(response.into_body().collect().await.unwrap().to_bytes(), "firstsecond");
        passed.push("existing_routes_privacy_and_streaming");

        for path in ["/mismatch", "/missing-upgrade"] {
            let response = request(proxy, "GET", &format!("/bitwarden/notifications{path}"), true).await;
            assert_eq!(response.status(), StatusCode::BAD_GATEWAY, "{path}");
            assert!(response.into_body().collect().await.unwrap().to_bytes().is_empty());
        }
        let response = request(proxy, "GET", "/e2e-http1/unsolicited", false).await;
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        assert!(response.into_body().collect().await.unwrap().to_bytes().is_empty());
        completed_rx.recv().await.unwrap().unwrap();
        let response = request(proxy, "GET", "/bitwarden/notifications/hang", true).await;
        assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
        assert!(response.into_body().collect().await.unwrap().to_bytes().is_empty());
        let response = request(proxy, "GET", "/bitwarden/notifications/reject", true).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(response.headers()["www-authenticate"], "Bearer");
        assert!(response.headers().get("upgrade").is_none());
        assert_eq!(response.into_body().collect().await.unwrap().to_bytes(), "denied");
        passed.push("invalid_refused_and_timed_out_handshakes");

        let mut io = websocket(proxy, "/hub?access_token=e2e", &websocket_frame(0x81, b"early", true)).await;
        expect_bytes(&mut io, &websocket_frame(0x81, b"ready", false)).await;
        expect_bytes(&mut io, &websocket_frame(0x81, b"early", false)).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        for (opcode, payload) in websocket_messages().into_iter().skip(1) {
            io.get_mut().write_all(&websocket_frame(opcode, &payload, true)).await.unwrap();
            let opcode = if opcode == 0x89 { 0x8a } else { opcode };
            expect_bytes(&mut io, &websocket_frame(opcode, &payload, false)).await;
        }
        drop(io);
        completed_rx.recv().await.unwrap().unwrap();
        passed.push("websocket_http1_early_data_large_frames_fragments_ping_close_and_lifetime");

        let mut io = websocket(proxy, "/half-close", b"").await;
        io.get_mut().write_all(b"last client bytes").await.unwrap();
        io.get_mut().shutdown().await.unwrap();
        let mut received = Vec::new();
        io.read_to_end(&mut received).await.unwrap();
        assert_eq!(received, b"last upstream bytes");
        completed_rx.recv().await.unwrap().unwrap();
        drop(websocket(proxy, "/disconnect", b"").await);
        completed_rx.recv().await.unwrap().unwrap();
        let mut io = websocket(proxy, "/upstream-close", b"").await;
        let mut received = Vec::new();
        io.read_to_end(&mut received).await.unwrap();
        assert_eq!(received, b"upstream goodbye");
        drop(io);
        completed_rx.recv().await.unwrap().unwrap();
        passed.push("half_close_and_disconnect_in_both_directions");

        shutdown.send(true).unwrap();
        proxy_task.await.unwrap();
        upstream_task.abort();
        http1_task.abort();
        passed.push("proxy_shutdown");
        let report = format!("{{\"status\":\"passed\",\"timestamp\":\"{}\",\"transport\":\"loopback HTTP -> production proxy -> TLS HTTP/2 or HTTP/1.1\",\"checks\":{:?}}}\n", now_iso(), passed);
        std::fs::write("target/bitwarden-e2e.json", &report).unwrap();
        println!("{report}");
    }).await.expect("end-to-end test exceeded 20 seconds");
}
