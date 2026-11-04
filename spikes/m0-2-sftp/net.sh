#!/bin/bash
# Server-side controls for the S-03 container, run as root inside WSL. The container has no tc, so it
# runs from the WSL side in the container's network namespace; nothing in the image changes.
#   net.sh lan             no shaping
#   net.sh wan             50 ms delay on the server's egress (plan M0-2 step 4)
#   net.sh blackhole       drop everything the server sends: a silently dead link
#   net.sh show            print the current qdisc
#   net.sh kill USER       kill USER's sshd sessions: an abruptly closed connection
#   net.sh freeze USER     stop USER's sshd sessions with SIGSTOP: TCP stays up, SSH goes silent
#   net.sh alive on|off    make sshd probe the keyonly user with keepalive@openssh.com every second
set -eu

container=fsm-test-sshd-1
pid=$(docker inspect --format '{{.State.Pid}}' "$container")
in_ns() { nsenter -t "$pid" -n "$@"; }
clear_qdisc() { in_ns tc qdisc del dev eth0 root 2>/dev/null || true; }
show() { in_ns tc qdisc show dev eth0; }
config=/etc/ssh/sshd_config
reload_sshd() { docker exec "$container" kill -HUP 1; }

case "${1:-show}" in
    lan) clear_qdisc; show ;;
    # The default netem limit of 1000 packets would drop traffic at this rate, and it would measure
    # netem's queue rather than a 50 ms link.
    wan) clear_qdisc; in_ns tc qdisc add dev eth0 root netem delay 50ms limit 200000; show ;;
    blackhole) clear_qdisc; in_ns tc qdisc add dev eth0 root netem loss 100%; show ;;
    show) show ;;
    kill) docker exec "$container" pkill -KILL -u "$2" ;;
    freeze) docker exec "$container" pkill -STOP -u "$2" ;;
    alive)
        # Only in the running container: the image's config stays as it is.
        docker exec "$container" sed -i '/^# m02-alive/,$d' "$config"
        if [ "$2" = on ]; then
            docker exec "$container" sh -c "printf '# m02-alive\nMatch User keyonly\n    ClientAliveInterval 1\n    ClientAliveCountMax 2\n' >> $config"
        fi
        reload_sshd
        ;;
    *) echo "usage: net.sh lan|wan|blackhole|show|kill USER|freeze USER|alive on|off" >&2; exit 2 ;;
esac
