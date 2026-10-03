BINARY = api-proxy
VERSION ?= dev
GOFLAGS = -trimpath -ldflags="-s -w -X main.version=$(VERSION)"

.PHONY: test build build-arm run clean

test:
	go test ./...
	go vet ./...

build: test
	CGO_ENABLED=0 GOOS=linux GOARCH=amd64 go build $(GOFLAGS) -o $(BINARY) .

build-arm: test
	CGO_ENABLED=0 GOOS=linux GOARCH=arm64 go build $(GOFLAGS) -o $(BINARY)-arm64 .

run:
	go run .

clean:
	rm -f $(BINARY) $(BINARY)-arm64

-include ops/local/Makefile
