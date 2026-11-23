#!/usr/bin/env python3
"""Probe only, never for main (#20504): round 2's `commands` clusters, one seed each, shrunk (parity_fuzz_probe)."""
from parity_fuzz_probe import run

SEEDS = [1017, 1002, 1008, 1042, 1022, 1086, 1010, 1033, 1059, 1442, 1107, 1175, 1173, 1075, 1001, 1219, 1140, 1149,
         1047, 1081, 1279, 1325, 1367, 1787, 1299, 1364, 1791, 1629, 1038]
PARITY_SHARDS = 29


def slices():
    return [['commands', '--ordinary', '--seed', str(seed), '--count', '1', '--shrink-rounds', '4'] for seed in SEEDS]


if __name__ == '__main__':
    run(slices())
