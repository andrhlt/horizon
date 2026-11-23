#!/usr/bin/env python3
"""Probe only, never for main (#20446): slices of tools/parity_fuzz.py as one sharded harness (parity_fuzz_probe)."""
from parity_fuzz_probe import run

PARITY_SHARDS = 49


def slices():
    out = []
    for family in ('items', 'blockdata', 'retype'):
        for index in range(16):
            out.append([family, '--seed', str(1 + 50 * index), '--count', '50', '--batch', '25', '--shrink-rounds', '1'])
    return out + [['--smoke']]


if __name__ == '__main__':
    run(slices())
