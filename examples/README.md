# 部署示例

代理程序只依赖监听地址和端口，下面的部署方式互相独立，不需要按顺序采用。

| 模式 | 入口 | 可选配置 |
|---|---|---|
| IP + 端口 | `http://VPS_IP:8000` | [systemd](./systemd/) |
| 单机域名 | Nginx 在同一台 VPS 终结 TLS | [nginx-single-node](./nginx-single-node/) |
| 前置机 + 落地机 | 前置机按 SNI 透传，落地机终结 TLS | [multi-node-sni](./multi-node-sni/) |

这些文件只展示配置边界，不负责申请证书、修改防火墙或覆盖服务器现有配置。
