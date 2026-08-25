# 单机 Nginx 示例

程序与 Nginx 运行在同一台 VPS：程序监听 `127.0.0.1:8000`，Nginx 负责公网域名和 TLS。

1. 将 `../nginx/api-proxy-locations.conf` 安装到 Nginx snippets 目录。
2. 把 `site.conf` 中的域名和证书路径替换成自己的值。
3. 将站点配置安装到自己的 Nginx 配置目录。
4. 执行 `nginx -t && nginx -s reload`。

证书可以由 Certbot、面板或其他证书管理工具提供，本项目不参与签发。
