#!/usr/bin/env python3
"""Fixture for tun::firewall::tests::kernel_traffic (run by that ignored Rust test).

Requires an isolated Linux user/network namespace, nftables, iptables-nft, UFW,
Mihomo, Python and util-linux. UFW configuration lives in a temporary directory.
Stdout/stdin carry reconciliation requests to the Rust implementation under test.
"""
import json
import os
from pathlib import Path
import shutil
import socket
import subprocess
import sys
import tempfile
import time
import urllib.request


def run(*args, **kwargs):
    result = subprocess.run(args, capture_output=True, text=True, **kwargs)
    if result.returncode:
        print(result.stdout + result.stderr, file=sys.stderr)
        result.check_returncode()
    return result.stdout


def manage(device):
    print(json.dumps({'device': device}), flush=True)
    response = json.loads(sys.stdin.readline())
    assert response['error'] is None, response


def report(message):
    print('PASS: ' + message, file=sys.stderr, flush=True)


def rules():
    return json.loads(run('nft', '-j', '-n', 'list', 'ruleset'))['nftables']


def owned():
    return [v['rule'] for v in rules() if v.get('rule', {}).get('comment') == 'omash-tun:0:input']


def traffic(address, udp=False):
    with socket.socket(socket.AF_INET6 if ':' in address else socket.AF_INET,
                       socket.SOCK_DGRAM if udp else socket.SOCK_STREAM) as sock:
        sock.settimeout(2)
        try:
            sock.connect((address, 18081 if udp else 18080))
            sock.sendall(b'omash-udp' if udp else b'GET / HTTP/1.0\r\n\r\n')
            reply = sock.recv(4096)
            return (b'omash-udp' if udp else b'omash-fixture') in reply
        except OSError:
            return False


def check_traffic():
    for address in ('198.51.100.1', '2001:db8:241::1'):
        for udp in (False, True):
            if not traffic(address, udp):
                print(json.dumps([v['rule'] for v in rules() if 'rule' in v and any(e.get('counter', {}).get('packets', 0) for e in v['rule'].get('expr', []))], indent=2), file=sys.stderr)
                raise AssertionError((address, udp))
    for address in ('198.51.100.99', '2001:db8:241::99'):
        assert not traffic(address), 'REJECT destination bypassed TUN'


assert os.geteuid() == 0
assert os.readlink('/proc/self/ns/net') != os.environ['OMASH_FIREWALL_PARENT_NETNS'], 'requires a disposable network namespace'
assert [link['ifname'] for link in json.loads(run('ip', '-j', 'link'))] == ['lo'], 'requires an empty test namespace'
assert 'nf_tables' in run('iptables', '--version'), 'requires iptables-nft'
for tool in ('nft', 'ip', 'nsenter', 'unshare', 'ufw', 'mihomo'):
    assert shutil.which(tool), 'missing ' + tool

# The server enters a second anonymous network namespace; neither namespace has
# an uplink to the host. No named namespaces, host routes or services are changed.
fixture = '''
import socket, threading, time
def serve(af, udp):
    s=socket.socket(af, socket.SOCK_DGRAM if udp else socket.SOCK_STREAM)
    if af==socket.AF_INET6: s.setsockopt(socket.IPPROTO_IPV6, socket.IPV6_V6ONLY, 1)
    if udp:
        s.setsockopt(socket.IPPROTO_IP if af==socket.AF_INET else socket.IPPROTO_IPV6,
                     8 if af==socket.AF_INET else socket.IPV6_RECVPKTINFO, 1)
    s.bind(('::' if af==socket.AF_INET6 else '0.0.0.0', 18081 if udp else 18080))
    if not udp: s.listen()
    while True:
        if udp:
            data,ancillary,_,addr=s.recvmsg(1024,128)
            s.sendmsg([data],ancillary,0,addr)
        else:
            c,_=s.accept()
            with c:
                c.recv(1024)
                c.sendall(b'HTTP/1.1 200 OK\\r\\nContent-Length: 13\\r\\nConnection: close\\r\\n\\r\\nomash-fixture')
for af in (socket.AF_INET,socket.AF_INET6):
    for udp in (False,True): threading.Thread(target=serve,args=(af,udp),daemon=True).start()
while True: time.sleep(60)
'''
peer = subprocess.Popen(['unshare', '--net', 'python3', '-c', fixture], stdout=sys.stderr)
children = [peer]
core = None
try:
    for _ in range(100):
        assert peer.poll() is None
        if os.readlink(f'/proc/{peer.pid}/ns/net') != os.readlink('/proc/self/ns/net'):
            break
        time.sleep(.02)
    else:
        raise AssertionError('peer namespace did not start')

    def peer_run(*args):
        return run('nsenter', f'--net=/proc/{peer.pid}/ns/net', *args)

    run('ip', 'link', 'set', 'lo', 'up')
    peer_run('ip', 'link', 'set', 'lo', 'up')
    run('ip', 'link', 'add', 'eth0', 'type', 'veth', 'peer', 'name', 'eth0', 'netns', str(peer.pid))
    for address in ('10.241.1.2/24', 'fd00:241::2/64'):
        run('ip', 'addr', 'add', address, 'dev', 'eth0', *(['nodad'] if ':' in address else []))
    for address in ('10.241.1.1/24', 'fd00:241::1/64'):
        peer_run('ip', 'addr', 'add', address, 'dev', 'eth0', *(['nodad'] if ':' in address else []))
    for address in ('198.51.100.1/32', '198.51.100.99/32', '2001:db8:241::1/128', '2001:db8:241::99/128'):
        peer_run('ip', 'addr', 'add', address, 'dev', 'lo', *(['nodad'] if ':' in address else []))
    run('ip', 'link', 'set', 'eth0', 'up')
    peer_run('ip', 'link', 'set', 'eth0', 'up')
    run('ip', 'route', 'add', 'default', 'via', '10.241.1.1')
    run('ip', '-6', 'route', 'add', 'default', 'via', 'fd00:241::1', 'dev', 'eth0')

    with tempfile.TemporaryDirectory(prefix='omash-firewall-') as directory:
        data = Path(directory)
        ufw_dir = data / 'etc/ufw'
        ufw_dir.mkdir(parents=True)
        (ufw_dir / 'applications.d').mkdir()
        (data / 'etc/default').mkdir()
        # Use UFW's own before rules and generated user rules with an isolated
        # datadir; never edit, enable, disable or reload the host's UFW instance.
        for name in ('before.rules', 'before6.rules'):
            shutil.copyfile('/etc/ufw/' + name, ufw_dir / name)
        for suffix, prefix in (('', 'ufw'), ('6', 'ufw6')):
            for stage in ('after', 'user'):
                chains = ''.join(f':{prefix}-{stage}-{hook} - [0:0]\n' for hook in ('input', 'output', 'forward'))
                (ufw_dir / f'{stage}{suffix}.rules').write_text('*filter\n' + chains + 'COMMIT\n')
        (ufw_dir / 'ufw.conf').write_text('ENABLED=no\nLOGLEVEL=off\n')
        (ufw_dir / 'sysctl.conf').write_text('')
        defaults = Path('/etc/default/ufw').read_text()
        (data / 'etc/default/ufw').write_text(defaults)
        # UFW stores its lock under its configured state directory.
        import ufw.common
        (data / ufw.common.state_dir.lstrip('/')).mkdir(parents=True, exist_ok=True)

        def ufw(*args):
            return run('ufw', '--rootdir=/', '--datadir=' + directory, *args)

        ufw('default', 'deny', 'incoming')
        ufw('default', 'allow', 'outgoing')
        ufw('--force', 'enable')
        assert 'Status: active' in ufw('status')
        for address in ('198.51.100.1', '2001:db8:241::1'):
            for udp in (False, True):
                for _ in range(3):
                    if traffic(address, udp):
                        break
                    time.sleep(.1)
                else:
                    print(run('ip', '-6', 'route'), file=sys.stderr)
                    print(run('ip', '-6', 'neigh'), file=sys.stderr)
                    raise AssertionError(('direct control', address, udp))
        report('real UFW enabled with default-deny input; direct controls reachable')

        config = {
            'mixed-port': 17897, 'external-controller': '127.0.0.1:19090',
            'secret': 'firewall-fixture', 'ipv6': True, 'log-level': 'error',
            'tun': {'enable': True, 'device': 'omash-fw-test', 'stack': 'mixed',
                    'auto-route': True, 'auto-detect-interface': True, 'auto-redirect': False,
                    'inet4-address': ['198.18.0.1/30'], 'inet6-address': ['fdfe:dcba:9876::1/126']},
            'rules': ['IP-CIDR,198.51.100.99/32,REJECT', 'IP-CIDR6,2001:db8:241::99/128,REJECT', 'MATCH,DIRECT'],
        }
        opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))

        def start(stack, device='omash-fw-test'):
            config['tun'].update(stack=stack, device=device)
            path = data / 'runtime.yaml'
            path.write_text(json.dumps(config))
            process = subprocess.Popen(['mihomo', '-d', directory, '-f', str(path)], stdout=sys.stderr, stderr=sys.stderr)
            children.append(process)
            for _ in range(100):
                assert process.poll() is None, 'Mihomo exited'
                try:
                    request = urllib.request.Request('http://127.0.0.1:19090/configs', headers={'Authorization': 'Bearer firewall-fixture'})
                    with opener.open(request, timeout=.2) as response:
                        runtime = json.load(response)
                    if runtime['tun']['enable']:
                        return process
                except OSError:
                    pass
                time.sleep(.05)
            raise AssertionError('Mihomo did not start')

        core = start('mixed')
        assert not traffic('198.51.100.1'), 'negative control: mixed unexpectedly passes the firewall'
        manage('omash-fw-test')
        check_traffic()
        report('mixed TCP timeout reproduced; Rust firewall management restores IPv4/IPv6 TCP/UDP')

        before = [r['handle'] for r in owned()]
        manage('omash-fw-test')
        assert [r['handle'] for r in owned()] == before, 'idempotent apply rewrote rules'
        ufw('reload')
        manage('omash-fw-test')
        check_traffic()
        assert 'Status: active' in ufw('status')
        run('iptables', '-S')
        run('ip6tables', '-S')
        report('real UFW reload, idempotence and iptables-nft compatibility')

        # A second native base chain must also accept: a separate early accept
        # cannot bypass this later drop. Only the test TUN interface is allowed.
        run('nft', 'add table inet omash_test')
        run('nft', 'add chain inet omash_test input { type filter hook input priority 10; policy drop; }')
        run('nft', 'add rule inet omash_test input iifname "lo" accept')
        run('nft', 'add rule inet omash_test input ct state established,related accept')
        run('nft', 'add rule inet omash_test input meta l4proto ipv6-icmp accept')
        assert not traffic('198.51.100.1')
        manage('omash-fw-test')
        check_traffic()
        with socket.socket() as listener:
            listener.bind(('0.0.0.0', 18123))
            listener.listen()
            probe = subprocess.run(['nsenter', f'--net=/proc/{peer.pid}/ns/net', 'python3', '-c',
                                    'import socket; socket.create_connection(("10.241.1.2",18123),1)'], capture_output=True)
            assert probe.returncode != 0, 'non-TUN inbound traffic was allowed'
        report('multiple input chains handled; ordinary inbound traffic remains denied')

        core.terminate()
        core.wait(timeout=10)
        core = None
        manage(None)
        assert not owned(), 'stop left firewall rules'
        core = start('system', 'renamed-tun')
        manage('renamed-tun')
        check_traffic()
        core.terminate()
        core.wait(timeout=10)
        core = None
        manage(None)
        assert not owned()
        report('system stack, custom interface name and stop cleanup')
        core = start('gvisor')
        check_traffic()
        assert not owned(), 'userspace stack requires no firewall exception'
        report('gvisor works without managed exceptions')
        core.terminate()
        core.wait(timeout=10)
        core = None
        # mips (Mihomo IP Stack) is another userspace stack, added in Mihomo v1.19.31.
        config['tun'].update(stack='mips', device='omash-fw-test')
        probe = data / 'mips.yaml'
        probe.write_text(json.dumps(config))
        validation = subprocess.run(['mihomo', '-t', '-d', directory, '-f', str(probe)],
                                    capture_output=True, text=True)
        if validation.returncode:
            reason = (validation.stdout + validation.stderr).strip().splitlines()[-1:]
            print('SKIP: the mips stack needs Mihomo v1.19.31 or later: ' + ''.join(reason),
                  file=sys.stderr, flush=True)
        else:
            core = start('mips')
            check_traffic()
            assert not owned(), 'userspace stack requires no firewall exception'
            report('mips works without managed exceptions')
finally:
    for process in reversed(children):
        if process.poll() is None:
            process.terminate()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()
