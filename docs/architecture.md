# 架构总览

## 项目定位

API Proxy 按路径前缀把请求转发到不同 AI/API 上游。Go 与 Rust 两套实现保持相同的路由、头处理、超时和错误语义。

程序本身是一个普通 HTTP 服务：

```text
客户端 -> BIND_HOST:PORT -> AI/API 上游
```

Nginx、TLS、SNI、前置机、落地机、防火墙和进程守护都位于程序边界之外。

## 目录结构

```text
.
├── main.go                         # Go 实现
├── rust/                           # Rust 实现
├── examples/
│   ├── systemd/                    # 可选进程守护
│   ├── nginx/                      # 共用路径白名单
│   ├── nginx-single-node/          # 可选单机 TLS 入口
│   └── multi-node-sni/             # 可选前置机 + 落地机
├── ops/local/                      # 维护者私人运维，Git 忽略
└── docs/
```

公开构建与运行不依赖 `ops/local/`。该目录存在时，根 Makefile 才会加载其中的私人运维命令。

## 代理语义

- 路径前缀改写到静态上游
- 请求侧剥离来源与隐私头
- 响应侧只做必要的 hop-by-hop 头清理
- 每个入站请求只向上游转发一次
- 拿不到上游响应时，只回空响应体的 `502/504`
- 不保存或解析客户端提供的 API 凭据

## Go 实现

核心文件是 `main.go`：

1. 读取运行时环境变量
2. 解析 `pathMappings` 为路由表
3. 使用 `httputil.ReverseProxy` 处理协议级代理语义
4. 维护共享 `http.Transport` 和响应缓冲池
5. 完成错误分类、结构化日志和优雅关闭

关键设计：

- 只匹配完整前缀段，目标 Scheme/Host 始终来自静态路由表
- 保留 `RawPath`，不改变 `%2F` 等转义路径语义
- 按路径段处理上游 base path 的追加与去重
- 剥离来源/隐私头，由标准库清理 hop-by-hop 头及 `Connection` 动态指定的头
- `PROXY_TIMEOUT_MS` 只约束等待上游响应头，不截断已经开始的流式响应
- 上游支持时使用 HTTP/2，不注入额外响应头

## Rust 实现

`rust/src/main.rs` 使用 hyper 1 + rustls 实现同一语义。完整对照与已知差异见 [rust/README.md](../rust/README.md)。加路由或修改代理行为时，两侧实现必须同步检查。

## 运行时环境变量

| 变量 | 默认值 | 说明 |
|---|---|---|
| `BIND_HOST` | `127.0.0.1` | 监听地址 |
| `PORT` | `8000` | 监听端口 |
| `PROXY_TIMEOUT_MS` | `300000` | 等待上游响应头的超时，不限制响应体总时长 |
| `PROXY_QUIET` | 未设置 | 置 `1` 关闭运行时日志 |
