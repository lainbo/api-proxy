# API Proxy

按路径前缀将请求反向代理到各 AI/API 上游服务。

- 使用 Go 标准库实现，入口为 `main.go`。
- 程序只依赖监听地址和端口；Nginx、TLS、SNI 与多机器拓扑均为可选部署方式。
- 项目保持“薄代理”语义：改写路径前缀、剥离来源/隐私头、清理必要的 hop-by-hop 头，并在拿不到上游响应时返回最薄的 `502/504`。

## 先看哪里

- 项目入口：[README.md](./README.md)
- 架构和关键文件：[docs/architecture.md](./docs/architecture.md)
- 本地开发和加路由：[docs/development.md](./docs/development.md)
- 部署配置与日常发布：[docs/deployment.md](./docs/deployment.md)
- 可选入口和服务器维护：[docs/operations.md](./docs/operations.md)
- 可选部署示例：[examples/README.md](./examples/README.md)

## 常见任务

### 加新路由

1. 修改 `main.go` 的 `pathMappings`。
2. 同步更新 `examples/nginx/api-proxy-locations.conf` 的可选入口白名单。
3. 运行 `make test`，再按 [docs/deployment.md](./docs/deployment.md) 发布。

### 修代理逻辑

- Go 主要文件：`main.go`
- 重点关注：路由边界、转义路径、来源头与 hop-by-hop 头清理、响应头超时、流式错误和错误回包

### 本地联调

- Go：`make run`

## 部署维护原则

- 公开默认方式是直接运行二进制，通过 `BIND_HOST` 和 `PORT` 控制监听。
- systemd、单机 Nginx 和多机器 SNI 只放在 `examples/`，互相独立，不得成为构建或运行前置条件。
- 私有基础设施、一键发布脚本和生成文件统一放在被 Git 忽略的 `ops/local/`。
- 根 Makefile 仅可选加载 `ops/local/Makefile`；公开构建与运行不得依赖该目录。
- Nginx 示例维持“API 前缀白名单 + 扫描路径直接 444 + 其他路径 404”。
- 不要把 Token、密码、私钥、证书内容或 DNS 凭据放入仓库。
