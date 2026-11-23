#!/usr/bin/env bash
set -euo pipefail

root=$(mktemp -d /var/tmp/nibrunner-customer-test.XXXXXXXX)
container="nibrunner-customer-test-${GITHUB_RUN_ID:?}-$GITHUB_RUN_ATTEMPT"
export NIBRUNNER_CUSTOMER_FIXTURE="$root"
cleanup() {
    result=$?
    if (( result != 0 )); then
        sudo journalctl --no-pager -n 80 -u nibrunner-fixture-writer -u nibrunner-fixture-mount \
            -u nibrunner-customer-zerofs@app-1 -u nibrunner-customer-mount@app-1 -u 'nibrunner-customer-nbd-*' -u nibrunner-fixture-recovery
    fi
    sudo systemctl stop nibrunner-customer-mount@app-1.service nibrunner-customer-zerofs@app-1.service 2>/dev/null || true
    sudo systemctl stop nibrunner-fixture-mount.service nibrunner-fixture-writer.service nibrunner-fixture-recovery.service 2>/dev/null || true
    docker rm -fv "$container" >/dev/null 2>&1 || true
    sudo rm -rf "$root"
}
trap cleanup EXIT
sudo apt-get install -y nbd-client fuse3
sudo modprobe nbd nbds_max=16 max_part=0
mkdir "$root/certs"
chmod 755 "$root"
openssl req -x509 -nodes -newkey rsa:2048 -days 1 -subj /CN=nibrunner-test-ca \
    -keyout "$root/ca-key" -out "$root/ca.crt" -addext basicConstraints=critical,CA:TRUE
openssl req -new -nodes -newkey rsa:2048 -subj /CN=localhost \
    -keyout "$root/certs/rustfs_key.pem" -out "$root/server.csr"
printf '%s\n' 'subjectAltName=IP:127.0.0.1' 'basicConstraints=critical,CA:FALSE' 'extendedKeyUsage=serverAuth' > "$root/extensions"
openssl x509 -req -days 1 -in "$root/server.csr" -CA "$root/ca.crt" -CAkey "$root/ca-key" \
    -CAcreateserial -extfile "$root/extensions" -out "$root/certs/rustfs_cert.pem"
docker run -d --name "$container" --user 0:0 -p 127.0.0.1::9000 \
    -e RUSTFS_ACCESS_KEY=fixture-access -e RUSTFS_SECRET_KEY=fixture-secret -e RUSTFS_TLS_PATH=/certs \
    -v "$root/certs:/certs:ro" rustfs/rustfs:1.0.0@sha256:8cc9801755448b71a786705ce76692c77e14936cccd87cf2fc31842e58f4d1ff
endpoint="https://$(docker port "$container" 9000/tcp)"
printf '%s' "$endpoint" > "$root/endpoint"
signed=(curl --fail --silent --show-error --retry-all-errors --retry 12 --retry-delay 1 --max-time 5 --cacert "$root/ca.crt" --aws-sigv4 aws:amz:us-east-1:s3 -u fixture-access:fixture-secret)
"${signed[@]}" -X PUT "$endpoint/customer-bucket"
just integration-guest customer_writer::
"${signed[@]}" "$endpoint/customer-bucket?list-type=2" | grep 'zerofs.key' >/dev/null
