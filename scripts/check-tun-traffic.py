#!/usr/bin/env python3
"""Verify TUN routing with negative controls and identify a real proxy leaf."""
import argparse
import ipaddress
import json
import os
import socket
import ssl
import time
import unittest
import urllib.request


NON_REMOTE_TYPES = {
    'direct', 'compatible', 'reject', 'rejectdrop', 'pass', 'dns',
    'selector', 'urltest', 'fallback', 'loadbalance', 'relay',
}


def confirms_remote_tun(connection, proxies):
    if 'tun' not in connection.get('metadata', {}).get('type', '').lower():
        return False
    chain = connection.get('chains', [])
    if not chain:
        return False
    # Mihomo appends each enclosing group after the actual outbound adapter.
    leaf = proxies.get(chain[0], {})
    kind = leaf.get('type', '').lower()
    return bool(kind) and kind not in NON_REMOTE_TYPES and 'all' not in leaf


def is_test_connection(connection, local, peer, host):
    metadata = connection.get('metadata', {})
    try:
        return (
            int(metadata.get('sourcePort', 0)) == local[1]
            and int(metadata.get('destinationPort', 0)) == peer[1]
            and ipaddress.ip_address(metadata['sourceIP']) == ipaddress.ip_address(local[0])
            and (metadata.get('host') == host
                 or ipaddress.ip_address(metadata['destinationIP']) == ipaddress.ip_address(peer[0]))
        )
    except (KeyError, ValueError):
        return False


def local_traffic(expect_tun):
    for family, allowed, denied in (
        (socket.AF_INET, '198.51.100.1', '198.51.100.99'),
        (socket.AF_INET6, '2001:db8:241::1', '2001:db8:241::99'),
    ):
        for transport in (socket.SOCK_STREAM, socket.SOCK_DGRAM):
            for address in (allowed, denied):
                received = b''
                with socket.socket(family, transport) as sock:
                    sock.settimeout(2)
                    try:
                        if transport == socket.SOCK_STREAM:
                            sock.connect((address, 18080))
                            sock.sendall(b'GET / HTTP/1.0\r\n\r\n')
                            while chunk := sock.recv(4096):
                                received += chunk
                        else:
                            sock.sendto(b'omash-udp', (address, 18081))
                            received = sock.recv(100)
                    except (TimeoutError, ConnectionError, OSError):
                        pass
                wanted = b'omash-fixture' if transport == socket.SOCK_STREAM else b'omash-udp'
                reachable = wanted in received
                assert reachable == (not expect_tun or address == allowed), (
                    f'family={family} transport={transport} address={address}: '
                    f'reachable={reachable}, expected TUN={expect_tun}'
                )
    print(f'PASS: IPv4/IPv6 TCP/UDP positive and negative controls (TUN={expect_tun})')


def remote_traffic():
    import tomllib
    with open(os.path.expanduser('~/.config/omash/config.toml'), 'rb') as source:
        config = tomllib.load(source)
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))

    def api(path):
        request = urllib.request.Request(
            config['controller'].rstrip('/') + path,
            headers={'Authorization': 'Bearer ' + config['secret']},
        )
        with opener.open(request, timeout=5) as response:
            return json.load(response)

    host = 'www.gstatic.com'
    with socket.create_connection((host, 443), timeout=15) as raw:
        with ssl.create_default_context().wrap_socket(raw, server_hostname=host) as connection:
            deadline = time.monotonic() + 5
            while time.monotonic() < deadline:
                matches = [
                    item for item in api('/connections').get('connections', [])
                    if is_test_connection(item, connection.getsockname(), connection.getpeername(), host)
                ]
                if matches:
                    assert len(matches) == 1, 'test TLS connection was not uniquely identified'
                    assert confirms_remote_tun(matches[0], api('/proxies')['proxies']), (
                        'test TLS connection must use both a TUN inbound and a remote proxy leaf'
                    )
                    print('PASS: exact TLS connection uses TUN and a remote proxy outbound')
                    return
                time.sleep(0.1)
            raise AssertionError('test TLS connection not visible in Mihomo')


class EvidenceTests(unittest.TestCase):
    def test_direct_behind_named_selector_is_not_a_remote_proxy(self):
        connection = {'metadata': {'type': 'Tun'}, 'chains': ['DIRECT', 'manual-selector']}
        self.assertFalse(confirms_remote_tun(connection, {
            'DIRECT': {'type': 'Direct'},
            'manual-selector': {'type': 'Selector', 'all': ['DIRECT']},
        }))

    def test_custom_direct_name_and_missing_leaf_are_rejected(self):
        connection = {'metadata': {'type': 'Tun'}, 'chains': ['ordinary', 'group']}
        self.assertFalse(confirms_remote_tun(connection, {'ordinary': {'type': 'Direct'}}))
        self.assertFalse(confirms_remote_tun(connection, {}))

    def test_tun_and_remote_proxy_must_belong_to_the_same_connection(self):
        proxies = {'node': {'type': 'Shadowsocks'}, 'DIRECT': {'type': 'Direct'}}
        self.assertFalse(confirms_remote_tun({'metadata': {'type': 'Tun'}, 'chains': ['DIRECT']}, proxies))
        self.assertFalse(confirms_remote_tun({'metadata': {'type': 'HTTP'}, 'chains': ['node']}, proxies))
        self.assertTrue(confirms_remote_tun({'metadata': {'type': 'Tun'}, 'chains': ['node', 'group']}, proxies))

    def test_connection_matches_socket_tuple_not_only_destination(self):
        connection = {'metadata': {
            'sourceIP': '10.0.2.100', 'sourcePort': '45000',
            'destinationIP': '203.0.113.1', 'destinationPort': '443', 'host': 'fixture.test',
        }}
        peer = ('203.0.113.1', 443)
        self.assertTrue(is_test_connection(connection, ('10.0.2.100', 45000), peer, 'fixture.test'))
        self.assertFalse(is_test_connection(connection, ('10.0.2.100', 45001), peer, 'fixture.test'))


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('mode', choices=['direct', 'tun', 'remote', 'self-test'])
    args = parser.parse_args()
    if args.mode == 'self-test':
        unittest.main(argv=[__file__])
    elif args.mode == 'remote':
        remote_traffic()
    else:
        local_traffic(args.mode == 'tun')
