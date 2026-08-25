# 可选入口与运维

本文只说明程序之外的部署边界。所有入口配置都是可选示例，不影响通过 IP + 端口直接运行代理。

## 进程管理

项目不要求固定的进程管理器。需要后台常驻时，可以选择：

- systemd
- Supervisor
- Docker 或其他容器运行时
- VPS 面板提供的进程守护

仓库只维护最小的 [systemd 示例](../examples/systemd/)，其他方式由部署者自行配置。

## 单机域名入口

[单机 Nginx 示例](../examples/nginx-single-node/) 展示以下链路：

```text
公网域名 -> 同机 Nginx TLS -> 127.0.0.1:8000
```

示例包含 API 路径白名单、扫描路径拦截和流式代理参数。证书可以来自任意证书管理工具；项目不调用 Certbot、不对接面板，也不保存证书路径之外的任何信息。

## 多机器 SNI 入口

[前置机 + 落地机示例](../examples/multi-node-sni/) 展示：

- 前置机 Nginx stream upstream
- SNI map 条目
- 落地机 TLS 站点
- 可选的 nftables 来源限制

这些都是独立片段。部署者应把需要的片段合入自己的现网配置，不要用示例覆盖完整的共享 Nginx 或防火墙配置。

## 证书边界

项目只在 Nginx 示例中保留两个占位符：

```text
<FULLCHAIN_PATH>
<PRIVATE_KEY_PATH>
```

申请、续期、DNS 验证、Token 权限和证书部署均由使用者自己的工具负责。仓库不包含证书签发脚本、DNS 服务商配置或自动续期逻辑。

## 私有运维

维护者自己的 IP、域名、SSH 端口、跳板、服务器目录、证书路径和一键发布脚本应放在：

```text
ops/local/
```

该目录被 Git 整体忽略。根目录 Makefile 只会在本机存在 `ops/local/Makefile` 时加载私人运维命令，公开仓库不依赖它。
