#!/usr/bin/env python3
"""Probe only, never for main (#20504): slices of tools/parity_fuzz.py as one sharded harness (parity_fuzz_probe)."""
from parity_fuzz_probe import run

PARITY_SHARDS = 17


def slices():
    return [['commands', '--ordinary', '--seed', str(5001 + 200 * index), '--count', '200', '--batch', '25',
             '--shrink-rounds', '0'] for index in range(16)] + [['--smoke']]


if __name__ == '__main__':
    run(slices())
