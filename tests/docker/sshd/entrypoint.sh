#!/bin/sh
# Installs the keys mounted at /keys with the permissions sshd insists on, then runs sshd.
set -eu

install -m 600 /keys/ssh_host_ed25519_key /etc/ssh/ssh_host_ed25519_key
install -m 644 /keys/ssh_host_ed25519_key.pub /etc/ssh/ssh_host_ed25519_key.pub
install -m 600 /keys/ssh_host_rsa_key /etc/ssh/ssh_host_rsa_key
install -m 644 /keys/user_ca.pub /etc/ssh/user_ca.pub

for u in tester keyonly; do
    install -d -m 700 -o "$u" -g "$u" "/home/$u/.ssh"
    cat /keys/id_ed25519.pub /keys/id_ed25519_pass.pub > "/home/$u/.ssh/authorized_keys"
    chown "$u:$u" "/home/$u/.ssh/authorized_keys"
    chmod 600 "/home/$u/.ssh/authorized_keys"
done

exec /usr/sbin/sshd -D -e "$@"
