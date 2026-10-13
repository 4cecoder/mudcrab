#!/usr/bin/env python3
"""Run Fiji's isolated headless Skyrim session and capture a verified loaded save."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import time

ROOT = Path('/home/taylor/Games/MudcrabComparison')

def run(*args, env=None, check=True):
    return subprocess.run(args, env=env, check=check, text=True, capture_output=True, timeout=45)

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--timeout', type=float, default=300)
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=False)
    tools = [Path(p) / 'bin' for p in (ROOT / 'artifacts/automation-tools.paths').read_text().splitlines()]
    def executable(name):
        return str(next(p / name for p in tools if (p / name).exists()))
    xdotool, xprop = executable('xdotool'), executable('xprop')
    ocr = Path((ROOT / 'artifacts/ocr-tool.path').read_text().strip()) / 'bin/tesseract'
    if not ocr.exists():
        raise RuntimeError('Pinned OCR tool is missing')
    manifest = json.loads((ROOT / 'session.json').read_text())
    expected = Path(manifest['save']['save_filename']).stem
    log = ROOT / 'compatdata/pfx/drive_c/users/steamuser/Documents/My Games/Skyrim Special Edition/SKSE/skse64.log'
    run('systemctl', '--user', 'start', 'mudcrab-comparison-display.service')
    env = os.environ.copy()
    deadline = time.monotonic() + args.timeout
    while time.monotonic() < deadline:
        display_file = ROOT / 'artifacts/display.env'
        if display_file.exists():
            values = dict(line.split('=', 1) for line in display_file.read_text().splitlines() if '=' in line)
            if values.get('DISPLAY') and Path('/tmp/.X11-unix/X' + values['DISPLAY'].split(':')[-1]).exists():
                env.update({k: values[k] for k in ['DISPLAY', 'WAYLAND_DISPLAY'] if k in values})
                break
        time.sleep(1)
    else:
        raise RuntimeError('Headless display failed to become ready')
    def capture(path):
        # X11 window imports can return an old swapchain image under Gamescope.
        # Request the current game plane from the compositor instead.
        source = Path('/tmp/gamescope.png')
        previous = source.stat().st_mtime_ns if source.exists() else None
        run(xprop, '-root', '-f', 'GAMESCOPECTRL_REQUEST_SCREENSHOT', '32c',
            '-set', 'GAMESCOPECTRL_REQUEST_SCREENSHOT', '1', env=env)
        capture_deadline = time.monotonic() + 10
        while time.monotonic() < capture_deadline:
            if source.exists() and source.stat().st_mtime_ns != previous:
                time.sleep(0.2)
                data = source.read_bytes()
                if data.startswith(b'\x89PNG\r\n\x1a\n') and data[-8:-4] == b'IEND':
                    path.write_bytes(data)
                    return
            time.sleep(0.1)
        raise RuntimeError('Gamescope did not produce a fresh complete PNG')
    run('systemctl', '--user', 'start', 'mudcrab-comparison-steam.service')
    # Pass the task-owned display to the game without changing the user manager environment.
    active = run('systemctl', '--user', 'is-active', 'mudcrab-comparison-game.service', check=False)
    if active.returncode:
        run('systemctl', '--user', 'reset-failed', 'mudcrab-comparison-game.service', check=False)
        run('systemctl', '--user', 'start', 'mudcrab-comparison-game.service')
    confirmed = False
    while time.monotonic() < deadline:
        window = run(xdotool, 'search', '--onlyvisible', '--name', '^Skyrim Special Edition$', env=env, check=False)
        if window.returncode:
            time.sleep(2)
            continue
        wid = window.stdout.splitlines()[-1]
        capture(args.output / 'current.png')
        text = run(str(ocr), str(args.output / 'current.png'), 'stdout', '--psm', '11').stdout.lower()
        # Only acknowledge the known achievements notice. Missing-content dialogs are never accepted.
        if not confirmed and 'achievements' in text and 'disabled' in text and 'continue loading' in text:
            run(xdotool, 'windowactivate', '--sync', wid, 'key', 'Return', env=env)
            confirmed = True
        if 'enable survival mode' in text:
            run(xdotool, 'windowactivate', '--sync', wid, 'key', 'Right', 'Return', env=env)
            time.sleep(2)
            continue
        content = log.read_text(errors='replace') if log.exists() else ''
        selected = 'save name is ' + expected in content
        loaded = 'dispatch message (3)' in content[content.find('save name is ' + expected):] if selected else False
        if selected and loaded and not any(t in text for t in ['continue loading', 'level 1', 'new save', 'missing content', 'survival mode']):
            # Let the initial terrain/LOD and mod initialization settle, then capture again.
            time.sleep(10)
            capture(args.output / 'loaded-save.png')
            final_text = run(str(ocr), str(args.output / 'loaded-save.png'), 'stdout', '--psm', '11').stdout.lower()
            if any(t in final_text for t in ['survival mode', 'continue loading', 'missing content']):
                continue
            data = (args.output / 'loaded-save.png').read_bytes()
            (args.output / 'skse64.log').write_text(content)
            receipt = {'kind': 'skyrim-headless-startup-smoke', 'save': manifest['save'],
                       'autoload_log_verified': True, 'post_load_message_verified': True,
                       'achievements_notice_acknowledged': confirmed,
                       'screenshot': 'loaded-save.png', 'screenshot_sha256': hashlib.sha256(data).hexdigest(),
                       'display': env['DISPLAY'], 'capture_backend': 'Gamescope base plane',
                       'matched_camera_verified': False}
            (args.output / 'receipt.json').write_text(json.dumps(receipt, indent=2) + '\n')
            print(json.dumps({'status': 'loaded-and-captured', 'output': str(args.output)}), flush=True)
            return
        time.sleep(3)
    (args.output / 'failure.json').write_text(json.dumps({'status': 'timeout', 'expected_save': expected}))
    raise RuntimeError('Save loading timed out; diagnostic screenshot retained')

if __name__ == '__main__':
    main()
