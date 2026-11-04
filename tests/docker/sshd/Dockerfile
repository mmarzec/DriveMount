# OpenSSH test server with one user per authentication method (see ../README.md).
FROM debian:bookworm-slim

RUN apt-get update \
 && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
      openssh-server libpam-google-authenticator expect oathtool \
 && rm -rf /var/lib/apt/lists/* /etc/ssh/ssh_host_*

RUN for u in tester keyonly certuser totpuser keytotp kbdpass; do useradd -m -s /bin/bash "$u"; done \
 && echo 'tester:tester' | chpasswd \
 && echo 'totpuser:totp-pass' | chpasswd \
 && echo 'keytotp:totp-pass' | chpasswd \
 && echo 'kbdpass:totp-pass' | chpasswd \
 && usermod -p '*' keyonly \
 && usermod -p '*' certuser

# TOTP second factor; nullok lets users without a secret file skip it.
COPY google_authenticator /home/totpuser/.google_authenticator
RUN install -m 400 -o keytotp -g keytotp /home/totpuser/.google_authenticator /home/keytotp/.google_authenticator \
 && chown totpuser:totpuser /home/totpuser/.google_authenticator \
 && chmod 400 /home/totpuser/.google_authenticator \
 && sed -i 's/^@include common-auth$/@include common-auth\nauth required pam_google_authenticator.so nullok/' /etc/pam.d/sshd

COPY sshd_config /etc/ssh/sshd_config
RUN sed '/^Subsystem/d' /etc/ssh/sshd_config > /etc/ssh/sshd_config_nosftp
RUN sed 's#^HostKey .*#HostKey /etc/ssh/ssh_host_rsa_key#' /etc/ssh/sshd_config > /etc/ssh/sshd_config_rsa
# Tester is locked into a tree where the home directory reads /C:/Users/tester.
RUN cp /etc/ssh/sshd_config /etc/ssh/sshd_config_winpaths \
 && printf 'Match User tester\n    ChrootDirectory /srv/winroot\n    ForceCommand internal-sftp -d /C:/Users/tester\n' >> /etc/ssh/sshd_config_winpaths \
 && mkdir -p /srv/winroot/C:/Users/tester /srv/winroot/C:/data \
 && chown tester:tester /srv/winroot/C:/Users/tester /srv/winroot/C:/data
# Things a test cannot make with RemoteFs: links, and a name that is not UTF-8.
RUN mkdir -p /srv/fixture/dir \
 && echo target > /srv/fixture/file \
 && ln -s file /srv/fixture/link-to-file \
 && ln -s dir /srv/fixture/link-to-dir \
 && ln -s absent /srv/fixture/broken-link \
 && : > "$(printf '/srv/fixture/caf\351')"
COPY entrypoint.sh check-auth.sh check-auth.exp /usr/local/bin/
RUN chmod 755 /usr/local/bin/entrypoint.sh /usr/local/bin/check-auth.sh /usr/local/bin/check-auth.exp \
 && mkdir -p /run/sshd

EXPOSE 22
ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
