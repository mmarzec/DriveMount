# ProFTPD mod_sftp test server: an SFTP v3 server without the OpenSSH extensions (see ../README.md).
FROM debian:trixie-slim

RUN apt-get update \
 && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
      proftpd-core proftpd-mod-crypto \
 && rm -rf /var/lib/apt/lists/*

RUN useradd -m -s /bin/bash tester \
 && echo 'tester:tester' | chpasswd

COPY proftpd.conf /etc/proftpd/proftpd.conf
COPY entrypoint.sh /usr/local/bin/
RUN mkdir -p /var/log/proftpd \
 && chmod 755 /usr/local/bin/entrypoint.sh \
 && chmod 644 /etc/proftpd/proftpd.conf

EXPOSE 22
ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
