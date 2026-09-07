# api-proxy (Rust 移植版)

Go 版 `main.go` 的 Rust 移植,语义尽量对齐。定位同样是"薄代理":
按路径前缀把请求转发到 AI/API 上游,只做路径改写、隐私头剥离、hop-by-hop
头清理和最薄的 502/504 错误回包。

## 技术栈

- `hyper 1.x` + `hyper-util`(连接池客户端、HTTP/1.1 服务端)
- `hyper-rustls`(系统 CA、TLS 1.2/1.3、ALPN 协商 HTTP/2)
- 无自动解压、无额外中间件 —— 对应 Go `DisableCompression: true`
- 全部逻辑在 `src/main.rs` 单文件,对应"`main.go` 是唯一实现"

## 命令

```bash
cd rust

cargo test              # 单元测试：路由边界/URL 改写/头清洗/错误分类
cargo run               # 本地运行,默认 127.0.0.1:8000
cargo build --release   # 产出 target/release/api-proxy(约 1.6MB)

# Linux amd64 静态构建
rustup target add x86_64-unknown-linux-musl
cargo build --release --target x86_64-unknown-linux-musl
```

也可以在仓库根目录用 `make rust-test` / `make rust-build` / `make rust-run`。
本地构建的版本为 `dev`；发布时通过根目录命令传入完整 Git commit SHA：

```bash
make rust-build-linux VERSION="$(git rev-parse HEAD)"
```

## 环境变量(与 Go 版一致)

| 变量 | 默认值 | 说明 |
|---|---|---|
| `BIND_HOST` | `127.0.0.1` | 监听地址；IP 直连时可显式设为 `0.0.0.0` |
| `PORT` | `8000` | 监听端口 |
| `PROXY_TIMEOUT_MS` | `300000` | 只约束等待上游响应头，不限制流式响应体传输 |
| `PROXY_QUIET` | 未设置 | 置 `1` 关闭全部运行时日志（生产启用） |

## 与 Go 版的语义对齐

已对齐的核心行为:

- 路由表与 Go `pathMappings` 完全一致;只匹配完整前缀段,
  `/openai@attacker.example/...` 直接 404
- 转义路径逐字节保留(`%2F`、`%25` 等)
- base path 去重拼接(`/openrouter/api/v1/x` → `/api/v1/x`,
  `/openrouter/apiv2/x` → `/api/apiv2/x`)
- 请求侧剥离 17 个来源/隐私头(X-Forwarded-*、X-Real-Ip、Via、Origin、
  Referer、CF/Fastly/GCP/Azure 的真实 IP 头等)
- hop-by-hop 清理含 `Connection` 点名的头;Upgrade 特例保留
- 响应侧只做 hop-by-hop 清理,不注入额外响应头
- `/health` 返回 `{"status":"ok","runtime":"rust","version":"...","uptime":N,"timestamp":"..."}`
- JSON 结构化日志:ts / level / msg / path / upstream / error / durationMs
- 上游拿不到响应时回空 body 的 502/504;优雅关闭(SIGINT/SIGTERM,30s)
- HTTP/2 上游(ALPN)、连接池(idle 90s、每 host 上限 10)、
  connect timeout 30s + TCP keepalive 30s

已知差异:

| 差异点 | Go 版 | Rust 版 |
|---|---|---|
| 连接超时错误码 | dial/TLS 超时归为 504 | 统一 502(仅响应头等待超时是 504) |
| 建连时限 | TCP 30s、TLS 握手 10s | TCP 30s，DNS/TCP/TLS 整体最多 40s |
| 全局空闲连接上限 | MaxIdleConns=100 | 无全局上限,每 host ≤10 |
| 客户端断连日志 | context 取消即记 | 无（无法与正常完成区分，不做检测） |
| 101 Upgrade 隧道 | 支持协议切换透传 | 不支持(AI API 场景用不到) |
| Expect: 100-continue | 显式超时配置 | 由 hyper 自动处理 |

## 测试方式

与 Go 版测试同构:`UpstreamTransport` trait 对应可替换的
`http.RoundTripper`,测试注入假实现断言"上游收到的请求"和
"客户端收到的响应",不依赖网络。

## 部署

在仓库根目录构建 Linux amd64 musl 静态二进制：

```bash
make rust-build-linux
```

产物位于 `rust/target/x86_64-unknown-linux-musl/release/api-proxy`。上传后只需设置 `BIND_HOST` 和 `PORT` 即可运行；配置与可选入口方式见 [部署文档](../docs/deployment.md)。
