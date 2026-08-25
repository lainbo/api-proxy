# 本地开发

## Go

```bash
# 默认监听 127.0.0.1:8000
make run

# 指定端口
PORT=9000 go run .

# 确实需要监听其他网卡时显式设置
BIND_HOST=0.0.0.0 PORT=9000 go run .

# 编译 linux/amd64
make build

# 编译 linux/arm64
make build-arm

# 测试与静态检查
go test ./...
go vet ./...
```

## 添加新路由

编辑 `main.go` 的 `pathMappings`：

```go
{"/新前缀", "https://目标API地址"},
```

Go 的 `pathMappings` 与 Rust 的 `PATH_MAPPINGS`（`rust/src/main.rs`）必须同步修改。使用可选 Nginx 示例的维护者还要同步更新 `examples/nginx/api-proxy-locations.conf`。部署方式见 [deployment.md](./deployment.md)。

新增或修改路由时，至少覆盖以下情况：

- 前缀本身和 `前缀/子路径`
- 相似但不属于该路由的路径，例如 `/openai-other`
- 上游自带 base path 时的追加与去重
- 包含 `%2F`、`%25` 的转义路径
- 查询参数原样保留

完成后运行：

```bash
make test
make rust-test
```
