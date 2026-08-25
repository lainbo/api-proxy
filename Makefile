BINARY = api-proxy
VERSION ?= dev
GOFLAGS = -trimpath -ldflags="-s -w -X main.version=$(VERSION)"

.PHONY: test build build-arm run clean rust-test rust-build rust-build-linux rust-run

test:
	go test ./...
	go vet ./...

build: test
	CGO_ENABLED=0 GOOS=linux GOARCH=amd64 go build $(GOFLAGS) -o $(BINARY) .

build-arm: test
	CGO_ENABLED=0 GOOS=linux GOARCH=arm64 go build $(GOFLAGS) -o $(BINARY)-arm64 .

run:
	go run .

rust-test:
	cd rust && API_PROXY_VERSION="$(VERSION)" cargo test

rust-build:
	cd rust && API_PROXY_VERSION="$(VERSION)" cargo build --release

rust-build-linux:
	docker run --platform linux/amd64 --rm -e API_PROXY_VERSION="$(VERSION)" -v "$(CURDIR)/rust:/io" -w /io rust:bookworm sh -c \
		'cargo test --locked && \
		apt-get update -qq && apt-get install -y -qq musl-tools >/dev/null && \
		rustup target add x86_64-unknown-linux-musl && \
		cargo build --release --locked --target x86_64-unknown-linux-musl'

rust-run:
	cd rust && API_PROXY_VERSION="$(VERSION)" cargo run

clean:
	rm -f $(BINARY) $(BINARY)-arm64

-include ops/local/Makefile
