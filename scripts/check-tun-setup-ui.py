#!/usr/bin/env python3
"""Turn Mihomo TUN on in the real TUI while its helper is missing, and answer the installation prompt.

usage: check-tun-setup-ui.py BINARY decline|cancel|install [PASSWORD_FILE]

decline  answers n in the confirmation: TUN stays off and sudo never runs.
cancel   answers y, then ends sudo's password prompt with EOF: setup fails and TUN stays off.
install  answers y and types the password read from PASSWORD_FILE: the helper is installed and TUN
         turns on.

Ratatui only sends cell diffs, so before a text is looked for the window is resized, which forces one
complete repaint. The window is 220 columns wide so that every notice fits on one line of the notice
panel; texts are compared without whitespace, as in check-tun-ui.py. The password is never printed
(sudo does not echo it).
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

SETTINGS_PAGE = b'7'
TUN_ROW = 5  # Keep core running, Start on login, System proxy, Allow LAN, IPv6, Mihomo TUN
ROWS, COLUMNS = 41, 220
ALT_ON = b'\x1b[?1049h'
ALT_OFF = b'\x1b[?1049l'
PASSWORD_PROMPT = b'password for'
RETURN_PROMPT = 'Press Enter to return to omash'
CONFIRMATION = 'Install TUN helper'
DECLINED = (
    'TUN stays off: the TUN helper is not installed. Press Enter on Mihomo TUN to install it, '
    'or run `omash tun setup` in a terminal.'
)
FAILED = 'TUN stays off: TUN helper setup failed'
RETRY = 'Press Enter on Mihomo TUN to try again, or run `omash tun setup` in a terminal.'
INSTALLED = 'TUN helper installed. Returning to omash turns TUN on.'
ACTIVE = 'TUN is active'


def config_path():
    base = os.environ.get('XDG_CONFIG_HOME') or os.path.join(os.path.expanduser('~'), '.config')
    return os.path.join(base, 'omash', 'config.toml')


def tun_enabled():
    with open(config_path(), encoding='utf-8') as source:
        match = re.search(r'^tun_enabled\s*=\s*(true|false)\s*$', source.read(), re.MULTILINE)
    assert match, 'tun_enabled is missing from config.toml'
    return match.group(1) == 'true'


def squeeze(raw):
    """Terminal output without escape sequences and whitespace."""
    plain = re.sub(rb'\x1b\[[0-?]*[ -/]*[@-~]', b'', raw)
    return b''.join(plain.split())


def squeezed(text):
    return b''.join(text.encode().split())


class Tui:
    def __init__(self, binary):
        self.pid, self.fd = pty.fork()
        if self.pid == 0:
            os.environ['TERM'] = 'xterm-256color'
            os.execv(binary, [binary])
        self.rows = ROWS
        self.resize()
        self.output = b''

    def resize(self):
        fcntl.ioctl(self.fd, termios.TIOCSWINSZ, struct.pack('HHHH', self.rows, COLUMNS, 0, 0))

    def pump(self, seconds):
        end = time.monotonic() + seconds
        while time.monotonic() < end:
            if select.select([self.fd], [], [], 0.1)[0]:
                try:
                    chunk = os.read(self.fd, 65536)
                except OSError:
                    return
                if not chunk:
                    return
                self.output += chunk

    def send(self, data):
        os.write(self.fd, data)

    def tail(self):
        return squeeze(self.output[-6000:]).decode('utf-8', 'replace')[-500:]

    def wait_for(self, done, seconds, failure):
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline:
            if done():
                return
            self.pump(0.2)
        assert done(), f'{failure}: {self.tail()}'

    def wait_for_text(self, text, since, seconds, failure):
        wanted = squeezed(text)
        self.wait_for(lambda: wanted in squeeze(self.output[since:]), seconds, failure)

    def screen_shows(self, text, seconds, failure):
        """Repaint the whole screen until it shows text."""
        wanted = squeezed(text)
        deadline = time.monotonic() + seconds
        # Let the TUI read the key sent just before. crossterm 0.29 drops the terminal's readiness when a
        # resize arrives in the same poll, and the key would then wait in the PTY for the next input.
        self.pump(0.5)
        while True:
            self.rows = ROWS + 1 if self.rows == ROWS else ROWS
            mark = len(self.output)
            self.resize()
            self.pump(1.5)
            if wanted in squeeze(self.output[mark:]):
                return
            assert time.monotonic() < deadline, f'{failure}: {self.tail()}'

    def quit(self):
        self.send(b'q')
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            done, raw = os.waitpid(self.pid, os.WNOHANG)
            if done:
                self.pid = 0
                status = os.waitstatus_to_exitcode(raw)
                assert status == 0, f'TUI exited with status {status}'
                return
            self.pump(0.1)  # keep draining the PTY so the exiting TUI cannot block on a full buffer
            time.sleep(0.01)
        raise AssertionError('TUI did not exit after q')

    def close(self):
        if self.pid:
            try:
                os.kill(self.pid, signal.SIGKILL)
                os.waitpid(self.pid, 0)
            except (ProcessLookupError, ChildProcessError):
                pass
        os.close(self.fd)


def leave_for_sudo(tui):
    """Confirm the installation and wait for sudo's prompt in the normal screen; returns its offset."""
    left = tui.output.count(ALT_OFF)
    mark = len(tui.output)
    tui.send(b'y')
    tui.wait_for(
        lambda: tui.output.count(ALT_OFF) > left and PASSWORD_PROMPT in tui.output[mark:],
        30,
        'the TUI did not leave its screen for a sudo password prompt',
    )
    return mark


def come_back(tui, since):
    """Answer the return prompt and wait until the TUI owns the screen again."""
    tui.wait_for_text(RETURN_PROMPT, since, 120, 'the TUI did not offer to return after setup')
    entered = tui.output.count(ALT_ON)
    tui.send(b'\n')
    tui.wait_for(lambda: tui.output.count(ALT_ON) > entered, 10, 'the TUI did not return to its screen')


def main():
    binary, mode = sys.argv[1], sys.argv[2]
    assert mode in ('decline', 'cancel', 'install'), 'second argument must be decline, cancel or install'
    password = None
    if mode == 'install':
        with open(sys.argv[3], 'rb') as source:
            password = source.read().strip()
        assert password, 'the password file is empty'
    assert not tun_enabled(), 'precondition: tun_enabled is already true in config.toml'

    tui = Tui(binary)
    try:
        # Keys typed before the alternate screen is up could be lost; wait for it explicitly.
        tui.wait_for(lambda: ALT_ON in tui.output, 15, 'TUI did not start')
        tui.pump(0.5)
        tui.send(SETTINGS_PAGE)
        tui.pump(0.8)
        for _ in range(TUN_ROW):
            tui.send(b'j')
            tui.pump(0.2)
        tui.send(b'\r')
        tui.screen_shows(CONFIRMATION, 10, 'the TUI did not ask before installing the helper')
        if mode == 'decline':
            tui.send(b'n')
            tui.screen_shows(DECLINED, 10, 'the TUI did not explain how to install the helper later')
            assert PASSWORD_PROMPT not in tui.output, 'sudo ran although the installation was declined'
            assert not tun_enabled(), 'declining the installation turned TUN on'
        elif mode == 'cancel':
            mark = leave_for_sudo(tui)
            # EOF at sudo's prompt: no password. Repeat it if sudo asks again.
            for _ in range(3):
                prompts = tui.output[mark:].count(PASSWORD_PROMPT)
                tui.send(b'\x04')
                tui.wait_for(
                    lambda: squeezed(RETURN_PROMPT) in squeeze(tui.output[mark:])
                    or tui.output[mark:].count(PASSWORD_PROMPT) > prompts,
                    30,
                    'sudo neither failed nor asked again after EOF',
                )
                if squeezed(RETURN_PROMPT) in squeeze(tui.output[mark:]):
                    break
            come_back(tui, mark)
            tui.screen_shows(FAILED, 10, 'the TUI did not report the failed setup')
            tui.screen_shows(RETRY, 10, 'the TUI did not explain how to install the helper later')
            assert not tun_enabled(), 'a failed setup turned TUN on'
        else:
            mark = leave_for_sudo(tui)
            tui.send(password + b'\n')
            tui.wait_for_text(INSTALLED, mark, 120, 'setup did not install the helper')
            come_back(tui, mark)
            tui.wait_for(tun_enabled, 10, 'the TUI did not turn TUN on after installing the helper')
            tui.screen_shows(ACTIVE, 60, 'the TUI did not report that TUN is active')
        tui.quit()
        print(f'TUI helper installation: {mode} behaved as expected; TUI exited 0')
    finally:
        tui.close()


if __name__ == '__main__':
    main()
