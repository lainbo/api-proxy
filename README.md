# API Proxy

一个保持“薄代理”语义的 AI/API 反向代理：按路径前缀选择上游，转发原始请求，并尽量不改变上游响应。

项目使用 Go 标准库实现，入口为 `main.go`。

## 支持的上游

当前包含 OpenAI、Anthropic、Gemini、OpenRouter、xAI、Telegram、Discord、Groq、Cohere、Hugging Face、Together、Novita、Portkey 和 Fireworks 等路径前缀。请求凭据只随请求转发，不写入项目配置。

### Bitwarden

通过 `/bitwarden/` 转发 Bitwarden 美国区官方云（`bitwarden.com`）的 API、身份验证、通知、图标和事件服务，账号、密码库和会员仍由官方服务承载。客户端登录页选择“自托管”后填写：

- 服务器 URL：`https://<代理域名>/bitwarden`
- 自定义环境中的网页密码库服务器 URL：`https://vault.bitwarden.com`

网页密码库保持直连官方，WebAuthn、Duo 两步验证和 Send 链接依赖它。这组路由原样转发 `Origin`，桌面端和 Safari 扩展需要它通过官方服务端的 CORS 预检；实时同步使用 WebSocket。

## 本地运行

```bash
make run
curl http://127.0.0.1:8000/health
```

健康检查会返回构建版本；本地构建默认显示 `version: "dev"`：

```json
{"status":"ok","version":"dev","uptime":12,"timestamp":"2026-08-25T12:00:00.000Z"}
```

## 部署到 VPS

程序本身只需要一个监听地址和端口，不依赖 Nginx、证书、面板或进程管理器。

```bash
make build
ssh root@<VPS_IP> 'install -d -m 755 /opt/api-proxy'
scp api-proxy root@<VPS_IP>:/opt/api-proxy/api-proxy
```

需要让发布产物可追溯到源码提交时，在构建阶段注入完整 Git commit SHA：

```bash
make build VERSION="$(git rev-parse HEAD)"
```

在 VPS 上直接运行：

```bash
chmod 755 /opt/api-proxy/api-proxy
BIND_HOST=0.0.0.0 PORT=8000 /opt/api-proxy/api-proxy
```

然后访问：

```bash
curl http://<VPS_IP>:8000/health
```

后台常驻、域名、TLS、前置机和落地机都属于可选部署方式：

- [systemd 常驻示例](examples/systemd/)
- [单机 Nginx + TLS](examples/nginx-single-node/)
- [前置机 + 落地机 SNI 透传](examples/multi-node-sni/)

三类入口互相独立，按需选择；systemd 可以与任一入口方式组合。完整说明见 [部署文档](docs/deployment.md)。

## 安全边界

- `BIND_HOST=0.0.0.0` 会让端口对外监听，是否开放以及允许哪些来源由部署者自己的防火墙决定。
- 直接通过公网 HTTP 访问会明文传输请求中的授权头；生产环境应使用可信私网、隧道或自行配置 TLS。
- 项目不保存服务端 API Key；客户端凭据只随对应请求转发。
- 项目不申请证书，也不读取 DNS Token、私钥或证书内容。

## License

[MIT](LICENSE)
