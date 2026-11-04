#!/bin/sh
# Installs the host key mounted at /keys with the permissions mod_sftp insists on, then runs ProFTPD.
set -eu

install -d -m 755 /etc/proftpd/keys
install -m 600 /keys/ssh_host_rsa_key /etc/proftpd/keys/ssh_host_rsa_key
# In the foreground, logging to the container's output.
exec /usr/sbin/proftpd --nodaemon --config /etc/proftpd/proftpd.conf
