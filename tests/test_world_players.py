import json
import os
import struct
import tempfile
import unittest
import uuid
from pathlib import Path

from tools import world_players as wp


def player_file(pos, dimension, marker):
    """A saved player: Pos, Dimension, a UUID and a string only a copy of this file can have."""
    double = lambda v: struct.pack('>d', v)
    string = lambda t: struct.pack('>H', len(t)) + t.encode()
    items = [
        (9, b'Pos', ('list', 6, [double(v) for v in pos])),
        (8, b'Dimension', string(dimension)),
        (8, b'Marker', string(marker)),
        (11, b'UUID', struct.pack('>i', 4) + struct.pack('>4i', 1, 2, 3, 4)),
    ]
    return wp.write_nbt(b'', ('compound', items))


class WorldPlayersTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.world = Path(self.tmp.name) / 'world'
        for sub in ('data', 'advancements', 'stats'):
            (self.world / 'players' / sub).mkdir(parents=True)
        self.players = []
        # Oldest first: p0 is the least recently active.
        for i, (pos, dim) in enumerate([((10, 64, 10), 'minecraft:overworld'), ((0, 70, 0), 'minecraft:the_end'),
                                        ((5, 80, 5), 'minecraft:overworld'), ((1, 2, 3), 'minecraft:the_nether'),
                                        ((7, 65, 9), 'minecraft:overworld')]):
            u = str(uuid.UUID(int=0x1000 + i))
            path = self.world / 'players' / 'data' / f'{u}.dat'
            path.write_bytes(player_file(pos, dim, f'marker{i}'))
            os.utime(path, (1000 + i, 1000 + i))
            (self.world / 'players' / 'stats' / f'{u}.json').write_text(json.dumps({'who': i}))
            self.players.append(u)

    def saved(self, name):
        data = (self.world / 'players' / 'data' / f'{wp.offline_uuid(name)}.dat').read_bytes()
        return wp.read_nbt(data)[1]

    def test_the_most_recent_players_go_to_the_bots_in_order(self):
        result = wp.assign(self.world, 3)
        self.assertEqual([result[f'Bot{i}']['source'] for i in range(3)],
                         [self.players[4], self.players[3], self.players[2]])
        self.assertEqual(result['Bot1']['dim'], 'minecraft:the_nether')
        self.assertEqual(result['Bot0']['pos'], (7, 65, 9))
        root = self.saved('Bot0')
        self.assertEqual(wp.describe(root), ((7, 65, 9), 'minecraft:overworld'))
        # What the bot holds is the player's: the marker is copied, the UUID is the bot's own.
        self.assertEqual(wp.field(root, 'Marker')[1][2:], b'marker4')
        ints = struct.unpack('>4i', wp.field(root, 'UUID')[1][4:])
        self.assertEqual(struct.pack('>4i', *ints), wp.offline_uuid('Bot0').bytes)
        stats = self.world / 'players' / 'stats' / f'{wp.offline_uuid("Bot0")}.json'
        self.assertEqual(json.loads(stats.read_text()), {'who': 4})

    def test_an_extra_takes_the_next_recent_player_in_its_dimension(self):
        result = wp.assign(self.world, 2, extras=[('Tester', 'minecraft:the_end')])
        self.assertEqual(result['Tester']['source'], self.players[1])
        self.assertEqual(result['Tester']['dim'], 'minecraft:the_end')
        self.assertNotIn(result['Tester']['source'], [result['Bot0']['source'], result['Bot1']['source']])

    def test_a_second_run_copies_players_not_bots(self):
        wp.assign(self.world, 2)
        again = wp.assign(self.world, 2)
        self.assertEqual({e['source'] for e in again.values()}, {self.players[4], self.players[3]})

    def test_a_damaged_file_is_skipped(self):
        newest = self.world / 'players' / 'data' / f'{self.players[4]}.dat'
        newest.write_bytes(b'not gzip')
        os.utime(newest, (5000, 5000))
        result = wp.assign(self.world, 2)
        self.assertEqual([result['Bot0']['source'], result['Bot1']['source']], [self.players[3], self.players[2]])

    def test_a_world_inside_the_repository_is_refused(self):
        self.assertTrue(wp.in_repository(wp.ROOT / 'tools' / 'not-ignored-world'))
        self.assertFalse(wp.in_repository(self.world))


if __name__ == '__main__':
    unittest.main()
