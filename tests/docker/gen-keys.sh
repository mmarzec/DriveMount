#!/usr/bin/env bash
# Generates throw-away keys for the test SFTP server into tests/docker/keys/ (git-ignored).
# Existing keys are kept, so the host key and known_hosts entries stay stable.
set -euo pipefail

dir="$(cd "$(dirname "$0")" && pwd)/keys"
mkdir -p "$dir"
cd "$dir"

gen() { [ -f "$1" ] || ssh-keygen -q -t ed25519 -N "$2" -C "$3" -f "$1"; }

gen ssh_host_ed25519_key "" fsm-test-host
gen user_ca "" fsm-test-user-ca
gen id_ed25519 "" fsm-test-key
gen id_ed25519_pass "test-passphrase" fsm-test-key-pass
gen id_cert "" fsm-test-cert
[ -f id_cert-cert.pub ] || ssh-keygen -q -s user_ca -I fsm-test-certuser -n certuser -V -5m:+520w id_cert.pub
