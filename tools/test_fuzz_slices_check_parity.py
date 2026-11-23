#!/usr/bin/env python3
"""Probe only, never for main (#20504): what parity-fuzz.yml's fuzz job does that no run has done yet. The game is
built, copied alone to another folder and run from there by `parity_fuzz_probe.py slices`, 8 slices side by side,
without what tools/parity.py adds to a harness's environment (the oracle shim, its game home, its Cargo folder).
Prints what the slices found and how long they took, and fails so the log is kept."""
import os
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

import fast_bins
import lab

ROOT = Path(__file__).resolve().parents[1]


def main():
    subprocess.check_call(fast_bins.command('horizon-client'), cwd=ROOT)
    away = Path(tempfile.mkdtemp(prefix='fuzz-game-')) / 'horizon-client'
    shutil.copy(lab.binary(lab.cargo_target() / 'fast', 'horizon-client'), away)
    away.chmod(0o755)
    env = {key: value for key, value in os.environ.items()
           if not key.startswith(('PARITY_', 'CARGO_')) and key not in ('HORIZON_HOME', 'HORIZON_NO_AUDIO')}
    if os.environ.get('PARITY_SHIM_REAL_PATH'):
        env['PATH'] = os.environ['PARITY_SHIM_REAL_PATH']
    out = ROOT / '.local' / 'parity-fuzz-check'
    shutil.rmtree(out, ignore_errors=True)
    steps = [('smoke', [sys.executable, 'tools/parity_fuzz.py', '--smoke', '--binary', str(away),
                        '--work-dir', str(ROOT / '.local' / 'parity-fuzz-smoke')]),
             ('slices', [sys.executable, 'tools/parity_fuzz_probe.py', 'slices', '--family', 'commands', '--seed', '9001',
                         '--count', '50', '--ordinary', '--binary', str(away), '--out', str(out), '--limit', '1500'])]
    for name, command in steps:
        started = time.monotonic()
        result = subprocess.run(command, cwd=ROOT, env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                                encoding='utf-8', errors='replace')
        print(f'=== {name}: exited {result.returncode} after {time.monotonic() - started:.0f} s ===\n{result.stdout}')
    for path in sorted(out.glob('slice-*/*')):
        text = path.read_text(encoding='utf-8', errors='replace')
        print(f'=== {path.relative_to(out)} ===\n{text if path.name != "report.json" else text[:2000]}\n')
    sys.exit(1)


if __name__ == '__main__':
    main()
