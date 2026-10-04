# systemd 示例

这不是程序运行的前置条件。需要后台常驻时，可以按自己的安装目录、监听地址和端口修改 `api-proxy.service`，然后安装：

```bash
install -m 644 api-proxy.service /etc/systemd/system/api-proxy.service
systemctl daemon-reload
systemctl enable --now api-proxy
systemctl status api-proxy
```

示例使用 `DynamicUser=yes`，由 systemd 分配无特权用户运行程序，二进制需保持 `755` 权限。监听端口低于 1024 时需另行授予 `CAP_NET_BIND_SERVICE`。

示例监听 `0.0.0.0:8000`，可直接通过 VPS IP 访问。开放公网端口前，请根据自己的场景设置主机防火墙。
