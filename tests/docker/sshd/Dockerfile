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
COPY entrypoint.sh check-auth.sh check-auth.exp /usr/local/bin/
RUN chmod 755 /usr/local/bin/entrypoint.sh /usr/local/bin/check-auth.sh /usr/local/bin/check-auth.exp \
 && mkdir -p /run/sshd

EXPOSE 22
ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
