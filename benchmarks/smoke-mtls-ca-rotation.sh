#!/usr/bin/env bash
set -euo pipefail

# Release-level smoke for identity-bound mTLS and no-downtime client-CA
# rotation. It uses loopback only and creates short-lived test credentials
# with the platform OpenSSL; production issuance remains an external concern.
export LANG=C.UTF-8
export LC_ALL=C.UTF-8

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
aspd_bin=${ASPD_BIN:-"$repo_root/target/release/aspd"}
asp_bin=${ASP_BIN:-"$repo_root/target/release/asp"}
workspace=$(mktemp -d "${TMPDIR:-/tmp}/asp-mtls-rotation.XXXXXX")
port=${ASP_MTLS_SMOKE_PORT:-4546}
health_port=${ASP_MTLS_SMOKE_HEALTH_PORT:-9456}
daemon_pid=""

if ! [[ "$port" =~ ^[1-9][0-9]*$ ]] || ((port > 65535)); then
  echo "ASP_MTLS_SMOKE_PORT must be an integer from 1 to 65535" >&2
  exit 2
fi
if ! [[ "$health_port" =~ ^[1-9][0-9]*$ ]] || ((health_port > 65535)); then
  echo "ASP_MTLS_SMOKE_HEALTH_PORT must be an integer from 1 to 65535" >&2
  exit 2
fi
if [[ "$port" == "$health_port" ]]; then
  echo "ASP_MTLS_SMOKE_PORT and ASP_MTLS_SMOKE_HEALTH_PORT must differ" >&2
  exit 2
fi

cleanup() {
  if [[ -n "$daemon_pid" ]]; then
    kill "$daemon_pid" 2>/dev/null || true
    wait "$daemon_pid" 2>/dev/null || true
  fi
  rm -rf -- "$workspace"
}
trap cleanup EXIT INT TERM

mkdir -p "$workspace/ca-bundle"
launcher="$workspace/launcher.sh"
printf '%s\n' '#!/bin/sh' 'exec "$@"' >"$launcher"
chmod 700 "$launcher"
cat >"$workspace/ca.cnf" <<'EOF'
[req]
distinguished_name = req_dn
prompt = no
[req_dn]
CN = ASP smoke CA
[v3_ca]
basicConstraints = critical,CA:TRUE
keyUsage = critical,keyCertSign,cRLSign
subjectKeyIdentifier = hash
authorityKeyIdentifier = keyid:always,issuer
EOF
openssl req -x509 -newkey rsa:2048 -nodes -days 2 -subj "/CN=ASP smoke CA 1" -config "$workspace/ca.cnf" -extensions v3_ca -keyout "$workspace/ca1.key.pem" -out "$workspace/ca1.pem" >/dev/null 2>&1
openssl req -x509 -newkey rsa:2048 -nodes -days 2 -subj "/CN=ASP smoke CA 2" -config "$workspace/ca.cnf" -extensions v3_ca -keyout "$workspace/ca2.key.pem" -out "$workspace/ca2.pem" >/dev/null 2>&1

cat >"$workspace/client.ext" <<'EOF'
basicConstraints=critical,CA:FALSE
keyUsage=critical,digitalSignature,keyEncipherment
extendedKeyUsage=clientAuth
EOF

make_client() {
  local name=$1 ca_name=$2 ca_key=$3 ca_cert=$4
  openssl req -newkey rsa:2048 -nodes -subj "/CN=ASP smoke ${name}" -keyout "$workspace/${name}.key.pem" -out "$workspace/${name}.csr.pem" >/dev/null 2>&1
  openssl x509 -req -days 2 -sha256 -in "$workspace/${name}.csr.pem" -CA "$ca_cert" -CAkey "$ca_key" -CAserial "$workspace/${ca_name}.srl" -CAcreateserial -extfile "$workspace/client.ext" -out "$workspace/${name}.pem" >/dev/null 2>&1
  openssl x509 -in "$workspace/${name}.pem" -outform DER -out "$workspace/${name}.der"
  openssl pkcs8 -topk8 -nocrypt -in "$workspace/${name}.key.pem" -outform DER -out "$workspace/${name}.key.der"
  openssl x509 -in "$ca_cert" -outform DER -out "$workspace/${ca_name}.der"
}

make_client client1 ca1 "$workspace/ca1.key.pem" "$workspace/ca1.pem"
make_client client2 ca2 "$workspace/ca2.key.pem" "$workspace/ca2.pem"
cp "$workspace/ca1.der" "$workspace/ca-bundle/ca1.der"

fingerprint() {
  openssl dgst -sha256 "$1" | awk '{print $NF}'
}
fp1=$(fingerprint "$workspace/client1.der")
fp2=$(fingerprint "$workspace/client2.der")
printf '{"alice":{"certificate_sha256":"%s","scopes":["*"]}}\n' "$fp1" >"$workspace/certificates.json"
chmod 600 "$workspace/certificates.json"

"$aspd_bin" \
  --production \
  --listen "127.0.0.1:$port" \
  --root "$workspace" \
  --client-ca "$workspace/ca-bundle" \
  --auth-certificates-file "$workspace/certificates.json" \
  --process-launcher "$launcher" \
  --require-process-launcher \
  --process-cpu-seconds 3600 \
  --exec-timeout-seconds 60 \
  --min-free-bytes 1 \
  --disable-port-forwarding \
  --health-listen "127.0.0.1:$health_port" \
  >"$workspace/aspd.log" 2>&1 &
daemon_pid=$!

doctor() {
  local name=$1
  "$asp_bin" \
    --cert "$workspace/.asp/server-cert.der" \
    --client-cert "$workspace/${name}.der" \
    --client-key "$workspace/${name}.key.der" \
    --session-file "$workspace/${name}.session.json" \
    doctor --strict "127.0.0.1:$port" \
    --ready-url "http://127.0.0.1:$health_port/ready" >/dev/null
}

for _ in $(seq 1 100); do
  if doctor client1 2>/dev/null; then
    break
  fi
  sleep 0.05
done
doctor client1

# Stage the replacement trust root and map before reloading. Both identities
# must work while the overlap is active.
cp "$workspace/ca2.der" "$workspace/ca-bundle/ca2.der"
printf '{"alice":{"certificate_sha256":"%s","scopes":["*"]},"bob":{"certificate_sha256":"%s","scopes":["*"]}}\n' "$fp1" "$fp2" >"$workspace/certificates.json.tmp"
chmod 600 "$workspace/certificates.json.tmp"
mv "$workspace/certificates.json.tmp" "$workspace/certificates.json"
kill -HUP "$daemon_pid"
for _ in $(seq 1 100); do
  if doctor client2 2>/dev/null; then
    break
  fi
  sleep 0.05
done
doctor client1
doctor client2

# Retire the old root only after the replacement has been verified.
rm "$workspace/ca-bundle/ca1.der"
printf '{"bob":{"certificate_sha256":"%s","scopes":["*"]}}\n' "$fp2" >"$workspace/certificates.json.tmp"
chmod 600 "$workspace/certificates.json.tmp"
mv "$workspace/certificates.json.tmp" "$workspace/certificates.json"
kill -HUP "$daemon_pid"
for _ in $(seq 1 100); do
  if doctor client2 2>/dev/null; then
    break
  fi
  sleep 0.05
done
doctor client2
if doctor client1 2>/dev/null; then
  echo "retired client CA unexpectedly authenticated" >&2
  exit 1
fi

printf 'ASP mTLS CA rotation smoke passed\n'
