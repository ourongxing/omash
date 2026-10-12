#!/usr/bin/env python3
"""Local dual-stack TCP/UDP/DNS peers for check-tun-linux; no internet dependency."""
import socket
import struct
import threading
import time


def dns_answer(data):
    end = 12
    while data[end]:
        end += data[end] + 1
    end += 1
    kind = struct.unpack('!H', data[end:end + 2])[0]
    question = data[12:end + 4]
    value = socket.inet_pton(socket.AF_INET6 if kind == 28 else socket.AF_INET,
                             '2001:db8:241::1' if kind == 28 else '198.51.100.1')
    answer = b'\xc0\x0c' + struct.pack('!HHIH', kind, 1, 60, len(value)) + value
    return data[:2] + struct.pack('!HHHHH', 0x8180, 1, 1, 0, 0) + question + answer


def serve(family, port, udp=False):
    sock = socket.socket(family, socket.SOCK_DGRAM if udp else socket.SOCK_STREAM)
    sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    if family == socket.AF_INET6:
        sock.setsockopt(socket.IPPROTO_IPV6, socket.IPV6_V6ONLY, 1)
    if udp:
        # Reply from the destination address of this datagram, not the primary
        # uplink address. Otherwise conntrack sees an unrelated inbound packet
        # and a default-deny firewall correctly drops our test fixture's reply.
        sock.setsockopt(socket.IPPROTO_IP if family == socket.AF_INET else socket.IPPROTO_IPV6,
                        8 if family == socket.AF_INET else socket.IPV6_RECVPKTINFO, 1)
    sock.bind(('::' if family == socket.AF_INET6 else '0.0.0.0', port))
    if not udp:
        sock.listen()
    while True:
        if udp:
            data, ancillary, _, addr = sock.recvmsg(65535, 128)
            sock.sendmsg([dns_answer(data) if port == 53 else data], ancillary, 0, addr)
        else:
            conn, _ = sock.accept()
            with conn:
                data = conn.recv(65535)
                if port == 53:
                    reply = dns_answer(data[2:])
                    conn.sendall(struct.pack('!H', len(reply)) + reply)
                else:
                    conn.sendall(b'HTTP/1.1 200 OK\r\nContent-Length: 13\r\nConnection: close\r\n\r\nomash-fixture')


for af in (socket.AF_INET, socket.AF_INET6):
    for port, udp in ((18080, False), (18081, True), (53, True), (53, False)):
        threading.Thread(target=serve, args=(af, port, udp), daemon=True).start()
while True:
    time.sleep(60)
