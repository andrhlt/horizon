import unittest

import re

import json
import subprocess
import tempfile
import tomllib
from pathlib import Path
from types import SimpleNamespace

from tools.server_bench import (DONE, PERF, PUMPKIN_READY, RUN_RECORD, TICK_AVERAGE, TICK_PERCENTILES, finish_profile,
                                pumpkin_config, report_line, start_profile, stopped_lines, work)


def run(server, bots, sent, new, waited=0.0, behaviour="mixed"):
    return {"server": server, "behaviour": behaviour, "bots": bots, "chunks_per_second": sent,
            "unique_per_second": new, "waited_ticks_per_second": waited}


class ServerBenchWorkTests(unittest.TestCase):
    def test_shares_of_the_busiest_server(self):
        [row] = work([run("horizon", 10, 98.2, 36.5), run("vanilla", 10, 24.3, 19.1, 12.0)])
        self.assertFalse(row["equal"])
        self.assertEqual(row["servers"]["horizon"]["chunks_per_second"], {"value": 98.2, "share": 1.0})
        self.assertEqual(row["servers"]["vanilla"]["chunks_per_second"]["share"], 0.247)
        self.assertEqual(row["servers"]["vanilla"]["waited_ticks_per_second"], 12.0)

    def test_equal_within_ten_percent_or_a_chunk(self):
        [row] = work([run("horizon", 25, 236, 28.9), run("vanilla", 25, 252, 28.9)])
        self.assertTrue(row["equal"])
        # One bot: 0.3 against 1.1 new chunks a second is within a chunk.
        [row] = work([run("horizon", 1, 9.9, 0.3), run("vanilla", 1, 9.9, 1.1)])
        self.assertTrue(row["equal"])

    def test_only_counts_run_on_several_servers(self):
        failed = dict(run("vanilla", 50, 0, 0), error="watchdog")
        self.assertEqual(work([run("horizon", 50, 450, 10), failed, run("horizon", 1, 9, 0.3)]), [])

    def test_each_round_is_compared_on_its_own(self):
        rows = work([dict(run("horizon", 20, 236, 111), round=0), dict(run("pumpkin", 20, 190, 69), round=0),
                     dict(run("horizon", 20, 237, 112), round=1), dict(run("pumpkin", 20, 237, 112), round=1)])
        self.assertEqual([(r["round"], r["equal"]) for r in rows], [(0, False), (1, True)])


class ServerBenchLogTests(unittest.TestCase):
    def test_every_server_says_it_is_up_the_same_way(self):
        for line in ['[05:16:10] [Server thread/INFO]: Done (9.762s)! For help, type "help"',
                     '[02:53:58 INFO]: Done (29.649s)! For help, type "help"']:
            self.assertTrue(re.search(DONE, line), line)
        self.assertFalse(re.search(DONE, '[02:53:58 INFO]: Preparing spawn area: Done (12%)'))

    def test_a_game_the_supervisor_restarted_fails_the_run(self):
        # From a regions run that stopped (the benchmark then measured the empty server it restarted).
        lines = ["[23:26:08] [Server thread/INFO]: ticking 150 regions for 150 players",
                 "[23:26:08] [Server thread/ERROR]: entity residency operation did not finish",
                 "[23:26:08] [Server thread/INFO]: Stopping server",
                 "[23:26:13] [Supervisor/WARN]: The game stopped unexpectedly (exit status: 1); restarting in 5 s"]
        [stopped] = stopped_lines(lines)
        self.assertIn("entity residency operation did not finish", stopped)
        self.assertEqual(stopped_lines(lines[:3]), [])

    def test_paper_answers_tick_query_as_vanilla_does(self):
        # Paper 26.3 build 205, from a smoke run of this tool.
        self.assertEqual(TICK_AVERAGE.search("Average time per tick: 14.4ms (Target: 50.0ms)").group(1), "14.4")
        self.assertEqual(TICK_PERCENTILES.search(
            "[02:54:10 INFO]: System chat: Percentiles: P50: 1.0ms P95: 34.6ms P99: 764.4ms. Sample: 100").groups(),
            ("1.0", "34.6", "764.4"))
    def test_pumpkin_is_up_once_it_listens(self):
        line = "16:39:19  INFO Server is now running. Connect using port: Java Edition: 127.0.0.1:39811"
        self.assertTrue(re.search(PUMPKIN_READY, line))


# A configuration in the shape Pumpkin writes (made up here: the keys this tool sets).
PUMPKIN_DEFAULTS = '''seed = "17"
default_gamemode = "Survival"
spawn_protection = 16

[logging]
color = true

[networking.query]
enabled = true
address = "0.0.0.0:25565"

[networking.java]
enabled = true
address = "0.0.0.0:25565"
encryption = true
online_mode = true
view_distance = 16
simulation_distance = 10

[networking.bedrock]
enabled = true
address = "0.0.0.0:19132"
online_mode = true
view_distance = 16

[plugins]
enabled = true

[telemetry]
enabled = true
endpoint = "https://example.org/heartbeat"
'''


class PumpkinConfigTests(unittest.TestCase):
    def test_the_benchmark_settings_on_loopback_with_the_extras_off(self):
        text = pumpkin_config(PUMPKIN_DEFAULTS, 27001, SimpleNamespace(seed=12345))
        self.assertIn('seed = "12345"', text)
        self.assertIn('default_gamemode = "Creative"', text)
        self.assertIn('spawn_protection = 0', text)
        java = text.split('[networking.java]')[1].split('[')[0]
        for line in ('address = "127.0.0.1:27001"', 'encryption = false', 'online_mode = false',
                     'view_distance = 10', 'simulation_distance = 10'):
            self.assertIn(line, java)
        self.assertIn('[networking.query]\nenabled = false\naddress = "127.0.0.1:27002"', text)
        self.assertIn('[networking.bedrock]\nenabled = false', text)
        self.assertIn('[plugins]\nenabled = false', text)
        self.assertIn('[telemetry]\nenabled = false\nendpoint = ""', text)
        self.assertNotIn('0.0.0.0:25565', text)
        # Only its own section's keys change: Bedrock keeps its (it is off).
        self.assertIn('address = "0.0.0.0:19132"\nonline_mode = true', text)

    def test_settings_absent_from_the_template_are_inserted(self):
        # A trimmed template lacks encryption, max_players and simulation_distance. Keeping them out would
        # leave Pumpkin's upstream defaults in place, so all declared values must still land in the output.
        minimal = '''seed = "old"
white_list = true

[networking.java]
address = "0.0.0.0:25565"
online_mode = true
view_distance = 16

[telemetry]
enabled = true
'''
        text = pumpkin_config(minimal, 25599, SimpleNamespace(seed=12345))
        config = tomllib.loads(text)  # Duplicate inserted keys also make parsing fail.
        java = config['networking']['java']
        self.assertEqual(config['seed'], '12345')
        self.assertFalse(config['white_list'])
        for key, value in {'address': '127.0.0.1:25599', 'online_mode': False, 'encryption': False,
                           'max_players': 200, 'view_distance': 10, 'simulation_distance': 10}.items():
            self.assertEqual(java[key], value)
        self.assertEqual(config['telemetry'], {'enabled': False, 'endpoint': ''})

    def test_a_section_missing_from_the_template_is_created(self):
        # No [networking.query] or [telemetry] at all: the whole section must be added with every key.
        minimal = '''seed = "12345"

[networking.java]
address = "0.0.0.0:25565"
'''
        config = tomllib.loads(pumpkin_config(minimal, 25599, SimpleNamespace(seed=12345)))
        self.assertEqual(config['networking']['java']['address'], '127.0.0.1:25599')
        self.assertEqual(config['networking']['query'], {'enabled': False, 'address': '127.0.0.1:25600'})
        self.assertEqual(config['telemetry'], {'enabled': False, 'endpoint': ''})
        self.assertFalse(config['plugins']['enabled'])
        self.assertFalse(config['networking']['java']['authentication']['enabled'])

    def test_empty_sections_keep_the_next_header_on_its_own_line(self):
        minimal = 'seed = "0"\n\n[networking.java]\n[telemetry]\n'
        config = tomllib.loads(pumpkin_config(minimal, 25599, SimpleNamespace(seed=12345)))
        self.assertEqual(config['seed'], '12345')
        self.assertEqual(config['networking']['java']['address'], '127.0.0.1:25599')
        self.assertEqual(config['networking']['java']['simulation_distance'], 10)
        self.assertEqual(config['telemetry'], {'enabled': False, 'endpoint': ''})

    def test_existing_keys_with_other_legal_spacing_are_replaced(self):
        minimal = 'seed\t=\t"0"\n[networking.java]\n\tmax_players=20\n  online_mode\t=\ttrue\n'
        config = tomllib.loads(pumpkin_config(minimal, 25599, SimpleNamespace(seed=12345)))
        self.assertEqual(config['seed'], '12345')
        self.assertEqual(config['networking']['java']['max_players'], 200)
        self.assertFalse(config['networking']['java']['online_mode'])

    def test_a_leading_table_keeps_inserted_root_settings_at_the_root(self):
        minimal = '[networking.java]\naddress = "0.0.0.0:25565"\n'
        config = tomllib.loads(pumpkin_config(minimal, 25599, SimpleNamespace(seed=12345)))
        self.assertEqual(config['seed'], '12345')
        self.assertNotIn('seed', config['networking']['java'])
        self.assertEqual(config['networking']['java']['address'], '127.0.0.1:25599')

    def test_commented_table_headers_are_not_appended_again(self):
        minimal = ('seed = "0"\n[networking.java] # Java listener\naddress = "0.0.0.0:25565"\n'
                   '[telemetry]\t# Telemetry\nenabled = true\n')
        config = tomllib.loads(pumpkin_config(minimal, 25599, SimpleNamespace(seed=12345)))
        self.assertEqual(config['networking']['java']['address'], '127.0.0.1:25599')
        self.assertEqual(config['telemetry'], {'enabled': False, 'endpoint': ''})


class FakeServer:
    """The console and the log of a server that prints what Horizon prints."""

    def __init__(self, lines=()):
        self.typed = []
        self.lines = list(lines)

    def console(self, line):
        self.typed.append(line)

    def since(self, n):
        return self.lines[n:]

    def wait_for(self, pattern, timeout):
        if not any(re.search(pattern, l) for l in self.lines):
            raise RuntimeError(f'no {pattern!r} within {timeout} s')


SAVED_LINE = ('[10:02:11] [Server thread/INFO]: Horizon: Saved reports/profile-20261108-100211Z.hzperf '
              '(the file stays on your computer)')


class ServerBenchProfileTests(unittest.TestCase):
    def test_the_report_is_asked_for_the_measure_window_with_the_given_flags(self):
        for profile, seconds, line in [('', 300, 'horizon-report 300'),
                                       ('--no-samples', 120, 'horizon-report 120 --no-samples'),
                                       # The game ends a report at 600 s.
                                       ('', 900, 'horizon-report 600')]:
            server = FakeServer()
            args = SimpleNamespace(profile=profile, seconds=seconds)
            self.assertTrue(start_profile(server, 'horizon', args))
            self.assertEqual(server.typed, [line])
            self.assertEqual(report_line(seconds, profile), line)

    def test_nothing_is_recorded_without_the_flag_or_on_another_server(self):
        server = FakeServer()
        self.assertFalse(start_profile(server, 'horizon', SimpleNamespace(profile=None, seconds=300)))
        for kind in ('vanilla', 'paper', 'fabric', 'pumpkin'):
            self.assertFalse(start_profile(server, kind, SimpleNamespace(profile='', seconds=300)))
        self.assertEqual(server.typed, [])

    def test_the_run_record_is_attached_to_the_capture_the_server_saved(self):
        record = {'server': 'horizon', 'bots': 25, 'mspt': {'average': 12.4, 'p95': 20.1}}
        calls = []

        def attach(argv, **kwargs):
            body = Path(argv[argv.index('--body') + 1])
            calls.append((argv, json.loads(body.read_text()), kwargs))
            return subprocess.CompletedProcess(argv, 0)

        with tempfile.TemporaryDirectory() as folder:
            directory = Path(folder)
            result = dict(record, bots_failed=[{'error': 'connect to 10.0.0.5:25599 refused'}])
            finish_profile(FakeServer(['starting', SAVED_LINE]), directory, result, run=attach)
        [(argv, body, kwargs)] = calls
        capture = directory / 'reports/profile-20261108-100211Z.hzperf'
        self.assertEqual(argv[:3], [PERF, 'attach', capture])
        self.assertEqual(argv[3:5], ['--name', RUN_RECORD])
        self.assertEqual(body, record)
        self.assertTrue(kwargs['check'])
        self.assertEqual(result['profile'], str(capture))

    def test_a_server_that_never_saves_leaves_the_run_standing_with_the_reason(self):
        calls = []
        result = {'server': 'horizon'}
        with tempfile.TemporaryDirectory() as folder:
            finish_profile(FakeServer(['starting']), Path(folder), result, run=lambda *a, **k: calls.append(a))
        self.assertEqual(calls, [])
        self.assertNotIn('profile', result)
        self.assertIn('Saved', result['profile_error'])

    def test_a_failing_attach_is_noted_and_the_run_stands(self):
        def fails(argv, **kwargs):
            raise subprocess.CalledProcessError(1, argv)

        result = {'server': 'horizon'}
        with tempfile.TemporaryDirectory() as folder:
            finish_profile(FakeServer([SAVED_LINE]), Path(folder), result, run=fails)
        self.assertNotIn('profile', result)
        self.assertIn('profile_error', result)


if __name__ == "__main__":
    unittest.main()


class ServerBenchWorldTests(unittest.TestCase):
    def test_region_landings_sum_the_profile_windows(self):
        from tools.server_bench import region_landings
        line = ('[20:37:12] [Server thread/INFO]: regions: wait 1.0 ms, global 2.0 ms, landing 30.0 ms (avg), landing ticks {n} '
                '(avg 91.0 ms, 1154.7 ms max); tick avg 91.0 p50 60.0 p95 245.5 p99 1142.8 max 1154.7 ms; per tick: split 25.5, '
                'other 1.0; 150 landings, 4 splits, landed for: {why}')
        lines = [line.format(n=199, why='region age 625, chat 7'), 'unrelated', line.format(n=1, why='chat 3, region wants 9'),
                 line.format(n=0, why='')]
        got = region_landings(lines)
        self.assertEqual((got['windows'], got['ticks'], got['landing_ticks'], got['landing_share']), (3, 600, 200, 0.333))
        self.assertEqual(got['reasons'], {'region age': 625, 'chat': 10, 'region wants': 9})
        self.assertEqual((got['landings'], got['splits']), (450, 12))
        self.assertIsNone(region_landings(['no profile here']))

    def test_problems_count_only_those_that_appear(self):
        from tools.server_bench import problems
        lines = ['x entity ids exhausted: 1', 'entity ids exhausted again', 'fine', 'Horizon crashed: entity pass is lifted']
        self.assertEqual(problems(lines), {'entity_ids_exhausted': 2, 'entity_pass_lifted': 1})

    def test_a_copied_server_toml_is_local_without_backups_or_a_webhook(self):
        from tools.server_bench import harden_toml
        text = ('[server]\nbind = ""\nonline_mode = true\n\n[backups]\nevery = "6h"\nkeep = 2\n\n[panel]\nbind = "127.0.0.1:8130"\n\n'
                '[discord]\nwebhook = "https://example.invalid/hook"\nenabled = true\n\n[regions]\nenabled = true\n')
        out = tomllib.loads(harden_toml(text))
        self.assertEqual((out['server']['bind'], out['server']['online_mode']), ('127.0.0.1', False))
        self.assertEqual((out['backups']['every'], out['backups']['keep']), ('0', 2))
        self.assertNotIn('discord', out)
        self.assertTrue(out['regions']['enabled'])
        # Bots on another machine need the server to listen beyond this one.
        self.assertEqual(tomllib.loads(harden_toml(text, bots_via=True))['server']['bind'], '')

    def test_a_world_is_copied_whole_and_without_its_lock(self):
        from tools.server_bench import copy_world
        with tempfile.TemporaryDirectory() as tmp:
            source = Path(tmp) / 'source'
            (source / 'region').mkdir(parents=True)
            (source / 'level.dat').write_bytes(b'level')
            (source / 'region' / 'r.0.0.mca').write_bytes(b'chunks')
            (source / 'session.lock').write_bytes(b'lock')
            target = Path(tmp) / 'run' / 'world'
            target.parent.mkdir()
            copy_world(source, target)
            self.assertEqual((target / 'region' / 'r.0.0.mca').read_bytes(), b'chunks')
            self.assertFalse((target / 'session.lock').exists())
            (target / 'level.dat').write_bytes(b'changed by the run')
            self.assertEqual((source / 'level.dat').read_bytes(), b'level')
            with self.assertRaises(RuntimeError):
                copy_world(Path(tmp) / 'region', Path(tmp) / 'elsewhere')

    def test_regions_alternate_in_the_opposite_order_every_second_round(self):
        from tools.server_bench import sides
        self.assertEqual([sides('horizon', 'alternate', r) for r in range(3)], [[False, True], [True, False], [False, True]])
        self.assertEqual(sides('horizon', 'on', 0), [None])
        self.assertEqual(sides('vanilla', 'alternate', 0), [None])
