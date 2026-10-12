#!/usr/bin/env python3
"""Exercise the real TUI in a PTY, then verify it exits cleanly without owning the core.

usage: check-tun-ui.py BINARY [TEXT ...]

Opens Settings and requires every TEXT on screen (whitespace-insensitive). Ratatui only sends cell
diffs, so a window resize forces one complete repaint before the screen is inspected.
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


def resize(fd, rows, columns):
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack('HHHH', rows, columns, 0, 0))


def normalize(raw):
    plain = re.sub(rb'\x1b\[[0-?]*[ -/]*[@-~]', b'', raw)
    return b''.join(plain.split())


def main():
    binary, expected = sys.argv[1], sys.argv[2:]
    pid, fd = pty.fork()
    if pid == 0:
        os.environ['TERM'] = 'xterm-256color'
        os.execv(binary, [binary])
    resize(fd, 40, 140)
    text = b''

    def pump(seconds):
        nonlocal text
        end = time.monotonic() + seconds
        while time.monotonic() < end:
            if select.select([fd], [], [], 0.1)[0]:
                try:
                    chunk = os.read(fd, 65536)
                except OSError:
                    return
                if not chunk:
                    return
                text += chunk

    try:
        deadline = time.monotonic() + 15
        while b'\x1b[?1049h' not in text and time.monotonic() < deadline:
            pump(0.2)
        assert b'\x1b[?1049h' in text, 'TUI did not start'
        pump(1.5)
        os.write(fd, b'7')  # Settings
        pump(1.0)
        resize(fd, 41, 140)  # forces a complete repaint
        mark = len(text)
        pump(2.5)
        normalized = normalize(text[mark:])
        assert b'MihomoTUN' in normalized, 'TUN setting was not rendered'
        for wanted in expected:
            assert b''.join(wanted.encode().split()) in normalized, (
                f'{wanted!r} not shown by the TUI (API refresh may have hidden it): '
                + normalized[-400:].decode('utf-8', 'replace')
            )
        os.write(fd, b'q')
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            done, status = os.waitpid(pid, os.WNOHANG)
            if done:
                pid = 0
                assert os.waitstatus_to_exitcode(status) == 0, 'TUI exited with an error'
                break
            pump(0.1)  # keep draining the PTY so the exiting TUI cannot block on a full buffer
            time.sleep(0.01)
        else:
            raise AssertionError('TUI did not exit')
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
