guest=egress-guest-$$
server=egress-server-$$
work=$(mktemp -d)
trap 'test -z "${listener:-}" || kill "$listener" 2>/dev/null || true; ip netns del "$guest"; ip netns del "$server"; rm -rf "$work"' EXIT
ip netns add "$guest"
ip netns add "$server"
ip link add nbr0 type veth peer name eth0 netns "$guest"
ip link add uplink type veth peer name eth0 netns "$server"
ip link set nbr0 address 02:00:00:00:00:01
ip -n "$guest" link set eth0 address 02:00:00:00:00:02
ip addr add 10.201.0.1/30 dev nbr0
ip -6 addr add fd00::1/64 dev nbr0 nodad
ip addr add 203.0.113.1/30 dev uplink
ip link set nbr0 up
ip link set uplink up
ip -n "$guest" link set eth0 up
ip -n "$guest" addr add 10.201.0.2/30 dev eth0
ip -n "$guest" -6 addr add fd00::2/64 dev eth0 nodad
# The guest-to-host refusal also covers IPv6 neighbour discovery.
ip -n "$guest" -6 neigh replace fd00::1 lladdr 02:00:00:00:00:01 nud permanent dev eth0
ip -6 neigh replace fd00::2 lladdr 02:00:00:00:00:02 nud permanent dev nbr0
ip -n "$guest" route add default via 10.201.0.1
ip -n "$server" link set eth0 up
ip -n "$server" addr add 203.0.113.2/30 dev eth0
sysctl -qw net.ipv4.ip_forward=1
printf '%s\n' "$ISOLATION" | nft -f -
printf '%s\n' "$INITIAL" | nft -f -
ip netns exec "$server" python3 - "$work/ready" <<'PY' &
import pathlib, socket, sys
with socket.socket() as listener:
    listener.bind(("203.0.113.2", 8080))
    listener.listen()
    pathlib.Path(sys.argv[1]).touch()
    for _ in range(3):
        connection, peer = listener.accept()
        with connection:
            assert peer[0] == "203.0.113.1"
            connection.sendall(b"ok")
PY
listener=$!
for _ in $(seq 1 100); do
    test ! -f "$work/ready" || break
    sleep 0.05
done
tcp() {
    ip netns exec "$guest" python3 -c 'import socket; s = socket.create_connection(("203.0.113.2", 8080), 3); assert s.recv(2) == b"ok"'
}
tcp
ip netns exec "$guest" python3 - <<'PY'
import errno, socket
with socket.socket(type=socket.SOCK_DGRAM) as udp:
    for _ in range(3):
        udp.sendto(b"test", ("203.0.113.2", 5353))
for family, address in [(socket.AF_INET, "10.201.0.1"), (socket.AF_INET6, "fd00::1")]:
    with socket.socket(family, socket.SOCK_DGRAM) as client:
        client.settimeout(3)
        client.connect((address, 8080))
        client.send(b"test")
        try:
            client.recv(1)
        except OSError as error:
            assert error.errno in (errno.ECONNREFUSED, errno.EHOSTUNREACH, errno.EACCES), error
        else:
            raise AssertionError("the host accepted a guest connection")
PY
printf '%s\n' "$ISOLATION" | nft -f -
tcp
printf '%s\n' "$REUSED" | nft -f -
tcp
wait "$listener"
listener=
printf '%s\n' "$LISTING" | nft -j -f -
printf '%s\n' "$DETACHED" | nft -f -
