# 部署

API Proxy 的运行边界只有二进制、监听地址和端口。域名、TLS、Nginx、SNI、落地机和进程守护都不是前置条件。

## 模式一：IP + 端口

这是默认部署方式，适合直接在一台 VPS 上运行。

直接公网 HTTP 会明文传输请求中的授权头；该模式应配合可信私网、隧道或部署者自己的 TLS 入口使用。

### Go

在开发机构建 Linux amd64 二进制：

```bash
make build
```

本地构建的 `/health` 版本默认为 `dev`。发布时可以注入完整 Git commit SHA：

```bash
make build VERSION="$(git rev-parse HEAD)"
```

创建远端目录并上传：

```bash
ssh root@<VPS_IP> 'install -d -m 755 /opt/api-proxy'
scp api-proxy root@<VPS_IP>:/opt/api-proxy/api-proxy
```

在 VPS 上运行：

```bash
chmod 755 /opt/api-proxy/api-proxy
BIND_HOST=0.0.0.0 PORT=8000 /opt/api-proxy/api-proxy
```

验证：

```bash
curl http://<VPS_IP>:8000/health
```

### Rust

使用 Docker 构建 Linux amd64 musl 静态二进制：

```bash
make rust-build-linux
```

发布时使用同一个 `VERSION` 参数注入完整 Git commit SHA：

```bash
make rust-build-linux VERSION="$(git rev-parse HEAD)"
```

产物位于：

```text
rust/target/x86_64-unknown-linux-musl/release/api-proxy
```

上传和运行方式与 Go 版相同。

### 后台常驻

程序不绑定特定进程管理器。可以使用 systemd、Supervisor、容器、面板或其他方式。仓库只提供一个可选的 [systemd 示例](../examples/systemd/)。

## 模式二：单机线路

程序和公网入口运行在同一台 VPS：

```text
客户端 -> Nginx/Caddy/面板 -> 127.0.0.1:8000 -> API 上游
```

此时建议让程序监听回环地址：

```bash
BIND_HOST=127.0.0.1 PORT=8000 /opt/api-proxy/api-proxy
```

域名、证书签发和 TLS 续期完全由部署者自己的入口软件处理。Nginx 示例见 [nginx-single-node](../examples/nginx-single-node/)。

## 模式三：前置机 + 落地机

只有需要多出口或按域名选择线路时才使用：

```text
客户端 -> 前置机 :443 SNI 透传 -> 落地机 TLS -> 127.0.0.1:8000
```

代理程序仍然只是运行在落地机上的 IP/端口服务。SNI、落地机证书、来源限制和防火墙属于独立基础设施，可选示例见 [multi-node-sni](../examples/multi-node-sni/)。

## 环境变量

| 变量 | 默认值 | 说明 |
|---|---|---|
| `BIND_HOST` | `127.0.0.1` | 监听地址；直连 VPS 时可设为 `0.0.0.0` |
| `PORT` | `8000` | 监听端口 |
| `PROXY_TIMEOUT_MS` | `300000` | 等待上游响应头的超时，不限制流式响应体总时长 |
| `PROXY_QUIET` | 未设置 | 置 `1` 关闭运行时日志 |

## 验收

```bash
curl http://<HOST>:<PORT>/health
curl -sS -o /dev/null -w '%{http_code}\n' \
  http://<HOST>:<PORT>/openai/v1/models
curl -sS -o /dev/null -w '%{http_code}\n' \
  http://<HOST>:<PORT>/definitely-not-a-route
```

预期：`/health` 为 `200`；无凭据 API 请求通常为 `401` 或 `403`；未登记路径为 `404`。

`/health` 同时返回 `runtime` 和构建阶段注入的 `version`，例如：

```json
{"status":"ok","runtime":"go","version":"<完整 Git commit SHA>","uptime":12,"timestamp":"2026-08-25T12:00:00.000Z"}
```
