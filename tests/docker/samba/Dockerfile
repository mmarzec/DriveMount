# Samba test server for the SMB backend (spec §10.2, M3). See ../README.md for users and shares.
FROM debian:bookworm-slim

RUN apt-get update \
 && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends samba \
 && rm -rf /var/lib/apt/lists/*

# Throw-away test credentials. smbuser1 and smbuser2 exist to reproduce Windows error 1219.
RUN for u in smbuser1 smbuser2; do useradd -M -s /usr/sbin/nologin "$u"; done \
 && (echo smbpass1; echo smbpass1) | smbpasswd -s -a smbuser1 \
 && (echo smbpass2; echo smbpass2) | smbpasswd -s -a smbuser2 \
 && mkdir -p /srv/a/sub/deeper /srv/b /srv/ro \
 && echo hello > /srv/a/hello.txt \
 && echo readonly > /srv/ro/readme.txt \
 && chmod 0777 /srv/a /srv/a/sub /srv/b

COPY smb.conf /etc/samba/smb.conf

EXPOSE 445
# Foreground, logging to stdout; no nmbd (NetBIOS does not cross the WSL NAT).
CMD ["smbd", "--foreground", "--no-process-group", "--debug-stdout", "--debuglevel=1"]
