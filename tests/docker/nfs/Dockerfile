# Linux knfsd test server for the NFS backend (spec §10.2, M4). See ../README.md for the exports.
# knfsd is the kernel's server: the container only configures it, so it runs privileged and on the
# host's network (nfsd is per network namespace, and Docker's port proxy would hide the client's
# source port, which the `secure` option checks).
FROM debian:bookworm-slim

RUN apt-get update \
 && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends nfs-kernel-server iproute2 \
 && rm -rf /var/lib/apt/lists/*

COPY exports /etc/exports
COPY entrypoint.sh /entrypoint.sh
RUN chmod 0755 /entrypoint.sh

EXPOSE 111 2049 20048
CMD ["/entrypoint.sh"]
