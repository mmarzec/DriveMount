#!/bin/sh
# Checks the interactive auth methods from inside the container (password, passphrase key, TOTP).
set -u

install -m 600 /keys/id_ed25519_pass /tmp/id_pass
fail=0

run() {
    name=$1; shift
    if check-auth.exp "$@" > /tmp/check.out 2>&1; then
        echo "ok   $name"
    else
        echo "FAIL $name"; cat /tmp/check.out; fail=1
    fi
}

run "password (tester)" tester tester
run "passphrase key (keyonly)" keyonly test-passphrase "" /tmp/id_pass
run "keyboard-interactive + TOTP (totpuser)" totpuser totp-pass JBSWY3DPEHPK3PXP

exit $fail
