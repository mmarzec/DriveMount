#!/bin/sh
# SPDX-License-Identifier: GPL-3.0-or-later
#
# The soak's network drops (M2-09): every EVERY seconds, drop all traffic to and from one container
# for FOR seconds with iptables in the WSL VM's DOCKER-USER chain, which every packet to a published
# port passes after Docker's DNAT. Run as root in WSL by soak.ps1; keeps WSL up as a side effect.
#
#   soak-net.sh <container-ip> <every-secs> <for-secs> <stop-file>
#   soak-net.sh clear <container-ip>
#
# Prints one line per change, "<UTC time> drop" or "<UTC time> restore", for the report.
set -u

rules() { # $1 = -I or -D
    iptables "$1" DOCKER-USER -d "$IP" -j DROP 2>/dev/null
    iptables "$1" DOCKER-USER -s "$IP" -j DROP 2>/dev/null
}

clear_rules() {
    while iptables -C DOCKER-USER -d "$IP" -j DROP 2>/dev/null; do iptables -D DOCKER-USER -d "$IP" -j DROP; done
    while iptables -C DOCKER-USER -s "$IP" -j DROP 2>/dev/null; do iptables -D DOCKER-USER -s "$IP" -j DROP; done
}

now() { date -u +%Y-%m-%dT%H:%M:%SZ; }

if [ "$1" = clear ]; then
    IP=$2
    clear_rules
    exit 0
fi

IP=$1 EVERY=$2 FOR=$3 STOP=$4
trap 'clear_rules; echo "$(now) restore (stopped)"; exit 0' INT TERM
clear_rules
echo "$(now) start ip=$IP every=${EVERY}s for=${FOR}s"

# Waits up to $1 seconds, a second at a time, and fails once the stop file is there.
wait_for() {
    n=0
    while [ "$n" -lt "$1" ]; do
        [ -e "$STOP" ] && return 1
        sleep 1
        n=$((n + 1))
    done
    return 0
}

while wait_for $((EVERY - FOR)); do
    rules -I
    echo "$(now) drop"
    wait_for "$FOR"
    stopped=$?
    clear_rules
    echo "$(now) restore"
    [ "$stopped" -ne 0 ] && break
done
clear_rules
echo "$(now) end"
