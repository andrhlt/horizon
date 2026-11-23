#!/usr/bin/env python3
"""Probe only, never for main (#20446): round 1's clusters, one seed each, shrunk (parity_fuzz_probe)."""
from parity_fuzz_probe import run

SEEDS = {'items': [9, 557, 553, 559, 24, 21, 23, 485, 707, 299, 691, 581],
         'blockdata': [69, 55, 56, 3, 351], 'retype': [11, 14, 44, 106, 124]}
PARITY_SHARDS = 22


def slices():
    return [[family, '--seed', str(seed), '--count', '1', '--shrink-rounds', '3']
            for family, seeds in SEEDS.items() for seed in seeds]


if __name__ == '__main__':
    run(slices())
