#!/usr/bin/env python3
"""Probe only, never for main (#20446): slices of tools/parity_fuzz.py as one sharded harness (parity_fuzz_probe)."""
from parity_fuzz_probe import run

PARITY_SHARDS = 29


def slices():
    out = []
    for family, shards, size in (('items', 16, 150), ('commands', 8, 100), ('blockdata', 4, 100)):
        for index in range(shards):
            out.append([family, '--ordinary', '--seed', str(1001 + size * index), '--count', str(size), '--batch', '25',
                        '--shrink-rounds', '0'])
    return out + [['--smoke']]


if __name__ == '__main__':
    run(slices())
