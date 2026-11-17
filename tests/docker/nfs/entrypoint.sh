#!/bin/sh
# Starts knfsd for NFSv3 over TCP with fixed ports: portmapper 111, NFS 2049, MOUNT 20048.
set -eu

for d in rw insecure ro denied; do
    mkdir -p "/srv/nfs/$d"
    mountpoint -q "/srv/nfs/$d" || mount -t tmpfs -o size=2g,mode=0777 "nfs-$d" "/srv/nfs/$d"
done
echo hello > /srv/nfs/ro/hello.txt
mkdir -p /srv/nfs/rw/sub && chmod 0777 /srv/nfs/rw/sub

mountpoint -q /proc/fs/nfsd || mount -t nfsd nfsd /proc/fs/nfsd

rpcbind -w
exportfs -ra
# v3 only, TCP only (no UDP), 8 threads.
rpc.nfsd --no-nfs-version 4 --no-udp 8
rpc.mountd --port 20048 --no-nfs-version 4 --no-udp

stop() {
    rpc.nfsd 0
    exportfs -ua
    # No pkill in this image.
    kill $(pidof rpc.mountd rpcbind) 2>/dev/null || true
    exit 0
}
trap stop TERM INT

exportfs -v
rpcinfo -p
echo "knfsd ready"
while :; do sleep 3600 & wait $!; done
