#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."

mkdir -p target
fixture_dir=$(mktemp -d "${TMPDIR:-/tmp}/api-proxy-e2e.XXXXXX")
trap 'rm -rf "$fixture_dir"' EXIT

openssl req -x509 -newkey rsa:2048 -nodes -days 1 \
  -subj /CN=localhost -addext 'subjectAltName=DNS:localhost,IP:127.0.0.1' \
  -addext 'basicConstraints=critical,CA:FALSE' \
  -keyout "$fixture_dir/key.pem" -out "$fixture_dir/cert.pem" 2>"$fixture_dir/openssl.log"
openssl x509 -in "$fixture_dir/cert.pem" -outform DER -out "$fixture_dir/cert.der"
openssl pkcs8 -topk8 -nocrypt -in "$fixture_dir/key.pem" -outform DER -out "$fixture_dir/key.der"

rm -f target/bitwarden-e2e.json
SSL_CERT_FILE="$fixture_dir/cert.pem" \
API_PROXY_E2E_CERT="$fixture_dir/cert.der" \
API_PROXY_E2E_KEY="$fixture_dir/key.der" \
cargo test --locked e2e::bitwarden_proxy -- --ignored --exact --nocapture \
  2>&1 | tee target/bitwarden-e2e.log
shasum -a 256 src/main.rs src/e2e.rs Cargo.lock tests/bitwarden-e2e.sh > target/bitwarden-e2e.sha256
