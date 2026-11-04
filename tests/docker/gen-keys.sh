#!/usr/bin/env bash
# Generates throw-away keys for the test SFTP server into tests/docker/keys/ (git-ignored).
# Existing keys are kept, so the host key and known_hosts entries stay stable.
set -euo pipefail

dir="$(cd "$(dirname "$0")" && pwd)/keys"
mkdir -p "$dir"
cd "$dir"

gen() { [ -f "$1" ] || ssh-keygen -q -t ed25519 -N "$2" -C "$3" -f "$1"; }

gen ssh_host_ed25519_key "" fsm-test-host
[ -f ssh_host_rsa_key ] || ssh-keygen -q -t rsa -b 3072 -N "" -C fsm-test-host-rsa -f ssh_host_rsa_key
gen user_ca "" fsm-test-user-ca
gen id_ed25519 "" fsm-test-key
gen id_ed25519_pass "test-passphrase" fsm-test-key-pass
gen id_cert "" fsm-test-cert
[ -f id_cert-cert.pub ] || ssh-keygen -q -s user_ca -I fsm-test-certuser -n certuser -V -5m:+520w id_cert.pub

# One login key per format the client must read (FR-SSH-02). "$@" holds the type and format options.
key() {
    local file=$1 passphrase=$2
    shift 2
    [ -f "$file" ] || ssh-keygen -q "$@" -N "$passphrase" -C "fsm-test-$file" -f "$file"
}

key id_rsa "" -t rsa -b 3072
key id_ecdsa "" -t ecdsa -b 256
key id_rsa_pem "" -t rsa -b 3072 -m PEM
key id_rsa_pem_pass "test-passphrase" -t rsa -b 3072 -m PEM
key id_rsa_pkcs8_pass "test-passphrase" -t rsa -b 3072 -m PKCS8
key id_cert_rsa "" -t rsa -b 3072
[ -f id_cert_rsa-cert.pub ] || ssh-keygen -q -s user_ca -I fsm-test-certuser-rsa -n certuser -V -5m:+520w id_cert_rsa.pub
# Too small for FR-SSH-02, though the server takes it.
key id_rsa_small "" -t rsa -b 1024
# Its certificate is for nobody, so the server refuses that but takes the plain key.
key id_wrongcert "" -t ed25519
[ -f id_wrongcert-cert.pub ] || ssh-keygen -q -s user_ca -I fsm-test-nobody -n nobody -V -5m:+520w id_wrongcert.pub
