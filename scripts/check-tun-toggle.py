#!/usr/bin/env python3
"""Toggle Mihomo TUN through the real TUI (Settings -> Mihomo TUN -> Enter), then quit with q.

usage: check-tun-toggle.py BINARY on|off

Ratatui sends cell diffs, so the screen text is not reliable evidence. The result is checked
functionally: config.toml must flip tun_enabled (and nothing else), and the TUI must exit 0.
Whether the supervisor then brings TUN up or down is verified by the caller.
"""
import fcntl
import os
import pty
import re
import select
import signal
import struct
import sys
import termios
import time

try:
    import tomllib
except ModuleNotFoundError:  # Python < 3.11: skip the unchanged-settings comparison
    tomllib = None

SETTINGS_PAGE = b'7'
TUN_ROW = 5  # Keep core running, Start on login, System proxy, Allow LAN, IPv6, Mihomo TUN


def config_path():
    base = os.environ.get('XDG_CONFIG_HOME') or os.path.join(os.path.expanduser('~'), '.config')
    return os.path.join(base, 'omash', 'config.toml')


def read_config():
    with open(config_path(), encoding='utf-8') as source:
        return source.read()


def tun_enabled(text):
    match = re.search(r'^tun_enabled\s*=\s*(true|false)\s*$', text, re.MULTILINE)
    assert match, 'tun_enabled is missing from config.toml'
    return match.group(1) == 'true'


def assert_only_tun_changed(before, after):
    """The first save rewrites a hand-written file in canonical form and adds defaults; values must not change."""
    if tomllib is None:
        return
    old, new = tomllib.loads(before), tomllib.loads(after)
    for key, value in old.items():
        if key != 'tun_enabled':
            assert new.get(key) == value, f'toggling TUN changed {key}: {value!r} -> {new.get(key)!r}'


def plain(raw):
    return re.sub(rb'\x1b\[[0-?]*[ -/]*[@-~]', b'', raw).decode('utf-8', 'replace')


def main():
    binary, wanted = sys.argv[1], sys.argv[2]
    assert wanted in ('on', 'off'), 'second argument must be on or off'
    want = wanted == 'on'
    before = read_config()
    assert tun_enabled(before) != want, f'precondition: tun_enabled is already {wanted} in config.toml'

    pid, fd = pty.fork()
    if pid == 0:
        os.environ['TERM'] = 'xterm-256color'
        os.execv(binary, [binary])
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack('HHHH', 40, 140, 0, 0))
    output = b''

    def pump(seconds):
        nonlocal output
        end = time.monotonic() + seconds
        while time.monotonic() < end:
            if select.select([fd], [], [], 0.1)[0]:
                try:
                    chunk = os.read(fd, 65536)
                except OSError:
                    return
                if not chunk:
                    return
                output += chunk

    try:
        # Keys typed before the alternate screen is up could be lost; wait for it explicitly.
        deadline = time.monotonic() + 15
        while b'\x1b[?1049h' not in output and time.monotonic() < deadline:
            pump(0.2)
        assert b'\x1b[?1049h' in output, 'TUI did not start: ' + plain(output)[-300:]
        pump(0.5)
        os.write(fd, SETTINGS_PAGE)
        pump(0.8)
        for _ in range(TUN_ROW):
            os.write(fd, b'j')
            pump(0.2)
        os.write(fd, b'\r')
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline and tun_enabled(read_config()) != want:
            pump(0.2)
        assert tun_enabled(read_config()) == want, (
            f'Settings -> Mihomo TUN -> Enter did not set tun_enabled={str(want).lower()}'
        )
        pump(1.0)  # let the TUI finish its own refresh before it is closed
        os.write(fd, b'q')
        status = None
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            done, raw = os.waitpid(pid, os.WNOHANG)
            if done:
                status = os.waitstatus_to_exitcode(raw)
                pid = 0
                break
            pump(0.1)  # keep draining the PTY so the exiting TUI cannot block on a full buffer
            time.sleep(0.01)
        assert status is not None, 'TUI did not exit after q'
        assert status == 0, f'TUI exited with status {status}'
        after = read_config()
        assert tun_enabled(after) == want, 'TUI exit changed tun_enabled again'
        assert_only_tun_changed(before, after)
        print(f'TUI Settings toggle set tun_enabled={str(want).lower()}; TUI exited 0')
    finally:
        if pid:
            try:
                os.kill(pid, signal.SIGKILL)
                os.waitpid(pid, 0)
            except (ProcessLookupError, ChildProcessError):
                pass
        os.close(fd)


if __name__ == '__main__':
    main()
