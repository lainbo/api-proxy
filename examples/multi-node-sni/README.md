# 前置机 + 落地机示例

该模式只适合需要把不同域名送往不同出口机器的场景：

```text
客户端 -> 前置机 :443 SNI 透传 -> 落地机 :8443 TLS -> 127.0.0.1:8000
```

前置机不解密 TLS，也不需要该域名的证书。把 `front-upstream.conf` 放入 Nginx `stream` 块，将 `front-map-entry.conf` 放入现有的 SNI `map`，不要覆盖可能承载其他业务的完整配置。

落地机负责：

1. 运行 api-proxy，并只监听 `127.0.0.1:8000`。
2. 使用 `landing-site.conf` 在 `8443` 终结 TLS。
3. 安装 `../nginx/api-proxy-locations.conf`。
4. 按需使用 `landing-firewall.nft`，只允许前置机访问 `8443`。

域名、证书路径、前置机 IP 和落地机 IP 都由部署者自行填写；本项目不申请或保存证书与 DNS 凭据。
