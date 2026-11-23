import tempfile
import unittest
from pathlib import Path

from tools import regions_world_check as rw
from tools import world_players as wp
from tests.test_world_players import player_file


def bot(dim, x):
    return {'dim': dim, 'pos': (x, 70, 0)}


class RegionsWorldCheckTests(unittest.TestCase):
    def test_the_teleport_plan_follows_the_reviews_table(self):
        sources = {'Bot0': bot(rw.OVERWORLD, 1000), 'Bot1': bot(rw.OVERWORLD, 1100), 'Bot2': bot(rw.OVERWORLD, -3000),
                   'Bot3': bot(rw.END, 0), 'Bot4': bot(rw.END, 10), 'Bot5': bot(rw.END, 20), 'Bot6': bot(rw.END, 30),
                   'Bot7': bot(rw.NETHER, 0)}
        plan = rw.teleport_plan(sources, {'Tester0': (0, 70, 0), 'Tester1': (0, 70, 0)})
        self.assertEqual([(who, dim) for who, _, dim, _ in plan],
                         [('Tester0', rw.OVERWORLD), ('Tester0', rw.END), ('Tester1', rw.END), ('Tester1', rw.OVERWORLD),
                          ('Tester1', rw.END), ('Tester1', rw.OVERWORLD)])
        # Every target is another bot, in the level the row names; the overworld ones are in other regions
        # (more than 400 blocks apart) and nobody is asked for twice.
        named = [t for _, _, _, t in plan if isinstance(t, str)]
        self.assertEqual(len(named), len(set(named)))
        self.assertEqual({n: sources[n]['dim'] for n in named[:1]}, {'Bot0': rw.OVERWORLD})
        self.assertEqual(sources[named[3]]['dim'], rw.OVERWORLD)
        self.assertGreater(abs(sources[named[0]]['pos'][0] - sources[named[3]]['pos'][0]), 400)
        # The End row that failed in the review: from the End to a player in the overworld.
        self.assertEqual(plan[3][1], f'tp {named[3]}')
        # The last row is typed with `execute in`, to a place.
        self.assertTrue(plan[5][1].startswith('execute in minecraft:overworld run tp Tester1'))

    def test_a_world_without_enough_players_in_the_end_has_no_plan(self):
        sources = {'Bot0': bot(rw.OVERWORLD, 1000), 'Bot1': bot(rw.OVERWORLD, -3000), 'Bot2': bot(rw.END, 0)}
        self.assertIsNone(rw.teleport_plan(sources, {'Tester0': (0, 70, 0), 'Tester1': (0, 70, 0)}))

    def test_a_players_snapshot_changes_with_the_place_and_not_with_the_file_time(self):
        with tempfile.TemporaryDirectory() as tmp:
            world = Path(tmp)
            (world / 'players' / 'data').mkdir(parents=True)
            path = world / 'players' / 'data' / f'{wp.offline_uuid("Bot0")}.dat'
            path.write_bytes(player_file((10, 64, 10), rw.OVERWORLD, 'a'))
            first = rw.player_snapshot(world, ['Bot0', 'Bot1'])
            self.assertEqual(list(first), ['Bot0'])
            self.assertEqual(first['Bot0'][:2], (rw.OVERWORLD, (10, 64, 10)))
            path.write_bytes(player_file((12, 64, 10), rw.END, 'a'))
            self.assertEqual(rw.player_snapshot(world, ['Bot0'])['Bot0'][:2], (rw.END, (12, 64, 10)))


if __name__ == '__main__':
    unittest.main()
