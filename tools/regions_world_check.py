#!/usr/bin/env python3
"""A scripted real-world check of region ticking, on a COPY of a world (docs/hosting.md, "Regions").

    python3 tools/regions_world_check.py check  WORLD --bots 100 --minutes 30 --out DIR
    python3 tools/regions_world_check.py make-world OUT --players 100      # a stand-in world, no real players

`check` copies WORLD (a world folder: it holds level.dat and players/), gives 100 bots the saved data of its most
recently active players (tools/world_players.py), starts Horizon's server on the copy with regions on
(127.0.0.1 only, backups and the Discord webhook off: the copy's settings are server_bench's, never the original's)
and checks, in order:

1. **Dead players.** Two bots that never respawn (`horizon-bots --no-respawn`) are killed; for 90 s nobody types a
   console command, and the regions must not land for `region wants` more than a handful of ticks.
2. **The teleports of the review's table**, typed by two operator bots as a player types them (never on the
   console, since a console command lands regions and would hide the path): overworld to another overworld region,
   overworld to a player in the End, End to another End region, End to a player in the overworld (the row that
   failed: "No entity was found"), End to a player in the overworld again from the End by `execute in`, with
   at least 40 s between a console command (the check that the player arrived, read after each teleport) and the
   next teleport.
3. **No `entity ids exhausted`, no panic, no failed region** for the whole run (`--minutes`, 30 by default), with
   the crowd walking on foot (`roam`) and saying a line a minute.
4. **A save and a restart.** `save-all flush` and `stop`; the log must not say `regions: save lost entities` or an
   error; the server starts again on the same folder, the bots stand where they were saved, and every player's
   dimension, position and inventory must be the same after a second save as before the stop.

The report (`report.json` in --out) has every check's outcome and numbers: the time the bots took to hold their
chunks, MSPT, the landing ticks and their reasons, the problems the log showed. The exit code is 1 when a check
failed. A check that could not run (the world has no player in the End, say) fails, it is not skipped.

`make-world` builds a stand-in world for a machine without the real one: a server on a fresh world, N bots (Seed0..,
so that `check`'s own Bot0.. clones do not overwrite them) spread
over a grid (so over many regions), a few of them sent to the End and the Nether, saved and stopped. It holds no
real player's data.

Nothing here prints or stores a real player's name: the map of which bot got whose data stays in
`bot-sources.json` beside the copy (tools/world_players.py), and this tool's report names only bots.
"""

import argparse
import hashlib
import io
import json
import re
import shutil
import subprocess
import sys
import threading
import time
from pathlib import Path
from types import SimpleNamespace

try:
    from . import lab, server_bench as sb, world_players as wp
except ImportError:
    import lab
    import server_bench as sb
    import world_players as wp

OVERWORLD, NETHER, END = 'minecraft:overworld', 'minecraft:the_nether', 'minecraft:the_end'
# Seconds between one teleport and the next, the first moment the arrival is read (a console command), and how
# long it is waited for: the last read ends at least STEP - READ_AFTER - READ_FOR = 45 s before the next teleport.
STEP, READ_AFTER, READ_FOR = 120, 30, 45
# How near the target a teleported player must be: the bots walk (roam) a few dozen blocks.
NEAR = 90.0
# Ticks a dead player's region may land for `region wants` in the quiet window (all windows summed).
DEAD_LANDINGS_ALLOWED = 5
POS = re.compile(r'has the following entity data: \[(-?[0-9.eE+-]+)d, (-?[0-9.eE+-]+)d, (-?[0-9.eE+-]+)d\]')
DIM = re.compile(r'has the following entity data: "([a-z_:]+)"')
COUNT = re.compile(r'#n has (\d+) \[chk\]')


class Check:
    def __init__(self, report):
        self.report = report
        self.results = []

    def add(self, name, ok, detail=''):
        self.results.append({'check': name, 'ok': bool(ok), 'detail': detail})
        print(f'{"PASS" if ok else "FAIL"} {name}: {detail}', flush=True)
        return ok


def properties(port, view, seed):
    props = dict(sb.PROPERTIES, **{'server-port': str(port), 'level-seed': str(seed), 'view-distance': str(view),
                                  'simulation-distance': str(view), 'gamemode': 'survival', 'force-gamemode': 'false',
                                  'allow-flight': 'true'})
    return ''.join(f'{k}={v}\n' for k, v in props.items())


def prepare(directory, world, port, view, seed, regions, ops=()):
    """A server folder with the copy of `world` in it (or none: a fresh world), ready to start; returns the
    command that starts it."""
    shutil.rmtree(directory, ignore_errors=True)
    directory.mkdir(parents=True)
    (directory / 'server.properties').write_text(properties(port, view, seed))
    (directory / 'eula.txt').write_text('eula=true\n')
    if ops:
        (directory / 'ops.json').write_text(json.dumps([
            {'uuid': str(wp.offline_uuid(n)), 'name': n, 'level': 4, 'bypassesPlayerLimit': False} for n in ops]))
    if world:
        copy_seconds = sb.copy_world(world, directory / 'world')
    else:
        copy_seconds = 0.0
    args = SimpleNamespace(world=str(world) if world else None, bots_via='', anticheat=False, dab=False,
                           regions_on=regions, port=port, pumpkin=None, heap='4g')
    return sb.server_command('horizon', directory, args), copy_seconds


def start_server(command, directory, log):
    server = sb.Server(command, directory, log)
    server.wait_for(sb.DONE, 900)
    return server


def start_bots(args, port, name, count, behaviour, seconds, out, extra=(), first=0, join_every=0.25):
    """horizon-bots; its per-second lines are collected, its summary goes to `out`. Returns (process, lines)."""
    command = [*sb.bots_command(SimpleNamespace(bots_via='', bots_bin=args.bots_bin)), '--server', f'127.0.0.1:{port}',
               '--bots', str(count), '--name', name, '--first-index', str(first), '--behaviour', behaviour,
               '--join-every', str(join_every), '--seconds', str(int(seconds)), '--view-distance', str(args.view),
               '--seed', str(args.seed), '--stdin-stop', '--report', str(out), *map(str, extra)]
    process = subprocess.Popen(command, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    lines = []
    threading.Thread(target=lambda: lines.extend(process.stdout), daemon=True).start()
    threading.Thread(target=process.stderr.read, daemon=True).start()
    return process, lines


def stop_bots(process):
    try:
        process.stdin.write('stop\n')
        process.stdin.flush()
    except (BrokenPipeError, ValueError, OSError):
        pass
    try:
        process.wait(timeout=60)
    except subprocess.TimeoutExpired:
        process.kill()


def read_console(server, command, pattern, timeout=10):
    """Runs a console command and returns the first match of `pattern` in what the server printed for it."""
    start = server.count()
    server.console(command)
    deadline = time.time() + timeout
    while time.time() < deadline:
        for line in server.since(start):
            if m := pattern.search(line):
                return m
        time.sleep(0.2)
    return None


def where(server, name):
    """(dimension, (x, y, z)) of an online player, read on the console; None when not found."""
    dim = read_console(server, f'data get entity {name} Dimension', DIM)
    pos = read_console(server, f'data get entity {name} Pos', POS)
    if not dim or not pos:
        return None
    return dim.group(1), tuple(float(pos.group(i)) for i in (1, 2, 3))


def far(a, b):
    return sum((x - y) ** 2 for x, y in zip(a, b)) ** 0.5


def entity_count(server):
    for line in ('scoreboard objectives add chk dummy', 'scoreboard players set #n chk 0'):
        server.console(line)
    time.sleep(1)
    server.console('execute as @e[type=!minecraft:player] run scoreboard players add #n chk 1')
    time.sleep(3)
    m = read_console(server, 'scoreboard players get #n chk', COUNT)
    return int(m.group(1)) if m else None


def player_snapshot(world, names):
    """{name: (dimension, position, inventory hash)} from the saved player files of the offline players `names`."""
    out = {}
    for name in names:
        path = world / 'players' / 'data' / f'{wp.offline_uuid(name)}.dat'
        if not path.exists():
            continue
        root = wp.read_nbt(path.read_bytes())[1]
        pos, dim = wp.describe(root)
        _, inventory = wp.field(root, 'Inventory')
        digest = hashlib.sha256()
        if inventory is not None:
            buf = io.BytesIO()
            wp.write_payload(buf, 9, inventory)
            digest.update(buf.getvalue())
        out[name] = (dim, pos, digest.hexdigest()[:16])
    return out


def teleport_plan(sources, tester_positions):
    """The review's table as (tester, command, expected dimension, expected place) from the cloned bots'
    positions; None when the world has too few players in the End or too few far apart in the overworld."""
    bots = {n: e for n, e in sources.items() if n.startswith('Bot')}
    overworld = [n for n, e in bots.items() if e['dim'] == OVERWORLD]
    end = [n for n, e in bots.items() if e['dim'] == END]
    start0 = tester_positions['Tester0']

    def pick(pool, avoid):
        # The first player at least 400 blocks from every place in `avoid` (another region).
        for n in pool:
            if all(far(bots[n]['pos'], a) > 400 for a in avoid):
                return n
        return None
    a = pick(overworld, [start0])
    a2 = pick([n for n in overworld if n != a], [start0, bots[a]['pos']] if a else [start0])
    if a is None or a2 is None or len(end) < 4:
        return None
    return [
        ('Tester0', f'tp {a}', OVERWORLD, a),
        ('Tester0', f'tp {end[0]}', END, end[0]),
        ('Tester1', f'tp {end[1]}', END, end[1]),
        ('Tester1', f'tp {a2}', OVERWORLD, a2),
        ('Tester1', f'tp {end[2]}', END, end[2]),
        ('Tester1', 'execute in minecraft:overworld run tp Tester1 100 120 100', OVERWORLD, (100.0, 120.0, 100.0)),
    ]


def check(args):
    out = Path(args.out).resolve()
    out.mkdir(parents=True, exist_ok=True)
    if wp.in_repository(out):
        raise SystemExit(f'{out} is inside the repository: it would hold a copy of player data')
    report = {'world_copy_seconds': None}
    checks = Check(report)
    port = lab.free_port()
    directory = out / 'server'
    minutes = args.minutes
    # Nothing starts before the copy is made and the players cloned.
    command, report['world_copy_seconds'] = prepare(directory, args.world, port, args.view, args.seed, args.regions == 'on',
                                                    ops=('Tester0', 'Tester1'))
    world = directory / 'world'
    extras = [('Tester0', OVERWORLD), ('Tester1', END), ('Dead0', OVERWORLD), ('Dead1', OVERWORLD)]
    cloned = wp.assign(world, args.bots, 'Bot', extras)
    report['players'] = {'bots': len([n for n in cloned if n.startswith('Bot')]),
                         'dimensions': {d: sum(e['dim'] == d for e in cloned.values()) for d in {e['dim'] for e in cloned.values()}}}
    plan = teleport_plan(cloned, {n: cloned[n]['pos'] for n in ('Tester0', 'Tester1')})
    checks.add('the world has players for the review\'s teleport table', plan is not None,
               f'{report["players"]["dimensions"]}')
    log = out / 'server.log'
    server = start_server(command, directory, log)
    failed = False
    try:
        names = [f'Bot{i}' for i in range(args.bots)]
        watch = minutes * 60
        crowd, crowd_lines = start_bots(args, port, 'Bot', args.bots, 'roam', watch + 900, out / 'crowd.json',
                                        ['--chat-every', 1200])
        started = time.time()
        try:
            report['load_seconds'] = sb.wait_ready(crowd_lines, args.bots, args.ready_timeout, crowd)
        except RuntimeError as e:
            checks.add('every bot holds its chunks', False, str(e))
            return finish(checks, report, out, server, [crowd], directory)
        checks.add('every bot holds its chunks', True, f'{report["load_seconds"]} s after the first joined')
        run_start = time.time()

        # 1. A dead player's region does not land for it tick after tick.
        dead, dead_lines = start_bots(args, port, 'Dead', 2, 'stand', 400, out / 'dead.json', ['--no-respawn'], join_every=1)
        time.sleep(20)
        for n in ('Dead0', 'Dead1'):
            server.console(f'kill {n}')
        time.sleep(15)
        mark = server.count()
        time.sleep(90)
        landings = sb.region_landings(server.since(mark))
        report['dead_landings'] = landings
        if args.regions == 'on':
            wants = (landings or {}).get('reasons', {}).get('region wants', 0)
            checks.add('dead players do not land their region every tick', landings is not None and wants <= DEAD_LANDINGS_ALLOWED,
                       f'region wants {wants} in {(landings or {}).get("ticks")} ticks')
        stop_bots(dead)

        # 2. The teleport table, typed by the testers themselves.
        if plan:
            quiet_from = time.time()
            testers = {}
            first = {'Tester0': 0, 'Tester1': 1}
            events = []
            warm = 40
            for k, (who, line, dim, target) in enumerate(plan):
                events.append((who, warm + k * STEP, line, dim, target))
            for who in ('Tester0', 'Tester1'):
                runs = [x for tag in ['--run'] for e in events if e[0] == who for x in (tag, f'{e[1] * 20}:{e[2]}')]
                testers[who] = start_bots(args, port, 'Tester', 1, 'stand', warm + len(plan) * STEP + 60, out / f'{who}.json',
                                          runs, first=first[who], join_every=1)
            t0 = time.time()
            rows = []
            for who, at, line, dim, target in events:
                time.sleep(max(0, t0 + at + READ_AFTER - time.time()))
                got = wanted = None
                ok = False
                for _ in range(READ_FOR // 5 + 1):
                    got = where(server, who)
                    wanted = bots_place(server, target) if isinstance(target, str) else target
                    ok = bool(got and wanted and got[0] == dim and far(got[1], wanted) <= NEAR)
                    if ok:
                        break
                    time.sleep(5)
                rows.append({'player': who, 'command': line if not isinstance(target, str) else f'tp <a bot in {dim}>',
                             'ok': ok, 'at': got[1] if got else None, 'dimension': got[0] if got else None})
                checks.add(f'teleport: {who} `{line if not isinstance(target, str) else "tp <bot>"}`', ok,
                           f'now in {got[0] if got else "?"}, {round(far(got[1], wanted)) if got and wanted else "?"} blocks from the target')
            report['teleports'] = rows
            for who in testers:
                stop_bots(testers[who][0])

        # 3. The run: no id exhaustion, panic or failed region.
        while time.time() - run_start < watch:
            time.sleep(30)
            if server.process.poll() is not None:
                checks.add('the server stays up', False, 'it exited')
                failed = True
                break
        report['problems'] = sb.problems(server.since(0))
        report['mspt'] = sb.tick_query(server)
        report['landings'] = sb.region_landings(server.since(0))
        checks.add(f'{minutes} minutes with no `entity ids exhausted`', not report['problems'].get('entity_ids_exhausted'),
                   f'{round((time.time() - run_start) / 60, 1)} minutes')
        bad = {k: v for k, v in report['problems'].items() if k != 'entity_ids_exhausted'}
        checks.add('no entity pass left lifted, panic, failed region or lost entities on a save', not bad, json.dumps(bad))

        # 4. Save, stop, start again, save again: nothing lost.
        before_count = entity_count(server)
        server.console('save-all flush')
        time.sleep(15)
        stop_bots(crowd)
        stopping = server.count()
        t = time.time()
        server.stop()
        report['stop_seconds'] = round(time.time() - t, 1)
        stop_lines = server.since(stopping)
        lost = [l for l in stop_lines if 'regions: save lost entities' in l or 'ERROR' in l or 'Horizon crashed' in l]
        checks.add('a save and a stop log no lost entities and no error', not lost, '; '.join(l.strip()[:160] for l in lost[:3]))
        snap_before = player_snapshot(world, names)
        server = start_server(command, directory, out / 'server-restart.log')
        crowd2, lines2 = start_bots(args, port, 'Bot', args.bots, 'stand', 900, out / 'crowd2.json')
        try:
            report['reload_seconds'] = sb.wait_ready(lines2, args.bots, args.ready_timeout, crowd2)
        except RuntimeError as e:
            checks.add('after the restart every bot holds its chunks', False, str(e))
            return finish(checks, report, out, server, [crowd2], directory)
        after_count = entity_count(server)
        server.console('save-all flush')
        time.sleep(15)
        stop_bots(crowd2)
        server.stop()
        snap_after = player_snapshot(world, names)
        differ = [n for n in snap_before if n not in snap_after or snap_before[n][0] != snap_after[n][0]
                  or snap_before[n][2] != snap_after[n][2] or far(snap_before[n][1], snap_after[n][1]) > 2]
        checks.add('every player is as saved after the restart (dimension, place within 2 blocks, inventory)', not differ,
                   f'{len(differ)} of {len(snap_before)} differ' + (f': {differ[:5]}' if differ else ''))
        if before_count is not None and after_count is not None:
            checks.add('entities are not lost by the restart (`@e`, 80 %)', after_count >= 0.8 * before_count,
                       f'{before_count} before, {after_count} after')
        report['entities'] = {'before': before_count, 'after': after_count}
        report['problems_restart'] = sb.problems(server.since(0))
    except Exception as e:  # a failed check, not a crash: the report says what ran
        checks.add('the check ran to its end', False, f'{type(e).__name__}: {e}')
        failed = True
    return finish(checks, report, out, server, [], directory)


def bots_place(server, name):
    got = where(server, name)
    return got[1] if got else None


def finish(checks, report, out, server, processes, directory):
    for p in processes:
        stop_bots(p)
    try:
        server.stop()
    except Exception:
        pass
    report['checks'] = checks.results
    report['ok'] = all(c['ok'] for c in checks.results)
    (out / 'report.json').write_text(json.dumps(report, indent=2) + '\n')
    # The copy of the world holds players' data: it does not outlive the check unless asked.
    print(f'{"ALL PASSED" if report["ok"] else "FAILED"}: {sum(c["ok"] for c in checks.results)} of {len(checks.results)} checks; {out / "report.json"}')
    return 0 if report['ok'] else 1


def make_world(args):
    """A stand-in world: N bots over a grid of cells, a few sent to the End and the Nether, saved and stopped."""
    out = Path(args.out).resolve()
    port = lab.free_port()
    directory = out / 'server'
    command, _ = prepare(directory, None, port, args.view, args.seed, args.regions == 'on')
    server = start_server(command, directory, out / 'make-world.log')
    try:
        server.console('gamerule respawn_radius 0')
        bots, lines = start_bots(args, port, 'Seed', args.players, 'scatter', args.players * 0.3 + args.settle + 300, out / 'make-world-bots.json',
                                join_every=0.3)
        time.sleep(args.players * 0.3 + args.settle)
        end, nether = max(1, args.players // 14), max(1, args.players // 30)
        moves = [(i, END, f'{(i % 7) * 40 - 120} 70 {(i % 5) * 40 - 80}') for i in range(args.players - end - nether, args.players - nether)]
        moves += [(i, NETHER, f'{(i % 7) * 40 - 60} 70 {(i % 5) * 40 - 60}') for i in range(args.players - nether, args.players)]
        # A console teleport can miss (a chunk not ready, a pull-back): send it again until the player is there.
        for attempt in range(5):
            pending = []
            for i, dim, at in moves:
                got = where(server, f'Seed{i}')
                if not got or got[0] != dim:
                    pending.append((i, dim, at))
            if not pending:
                break
            for i, dim, at in pending:
                server.console(f'execute in {dim} run tp Seed{i} {at}')
            time.sleep(15)
        # The bots leave before the save: a player is saved where the server has them, in their own level.
        stop_bots(bots)
        time.sleep(5)
        server.console('save-all flush')
        time.sleep(20)
    finally:
        server.stop()
    world = directory / 'world'
    target = Path(args.world_out).resolve() if args.world_out else out / 'world'
    if target != world:
        shutil.rmtree(target, ignore_errors=True)
        shutil.move(str(world), str(target))
    print(f'stand-in world in {target} ({len(list((target / "players" / "data").glob("*.dat")))} players saved)')
    return 0


def main():
    parser = argparse.ArgumentParser(description=__doc__.split('\n\n')[0])
    sub = parser.add_subparsers(dest='command', required=True)
    common = argparse.ArgumentParser(add_help=False)
    common.add_argument('--out', type=Path, required=True, help='where logs, the server folder and the report go (outside the repository)')
    common.add_argument('--view', type=int, default=8)
    common.add_argument('--seed', type=int, default=12345)
    common.add_argument('--regions', choices=['on', 'off'], default='on')
    common.add_argument('--bots-bin', default='', help='horizon-bots (default: beside horizon-server, HORIZON_BENCH_BIN)')
    c = sub.add_parser('check', parents=[common])
    c.add_argument('world', type=Path, help='the world folder to copy (never changed)')
    c.add_argument('--bots', type=int, default=100)
    c.add_argument('--minutes', type=int, default=30)
    c.add_argument('--ready-timeout', type=int, default=1200)
    m = sub.add_parser('make-world', parents=[common])
    m.add_argument('--players', type=int, default=100)
    m.add_argument('--world-out', type=Path, default=None)
    m.add_argument('--settle', type=int, default=600,
                   help='seconds the bots stand in their cells before the save, so the server generates and saves '
                        'the chunks round them (a world a bench loads from disk, not generates)')
    args = parser.parse_args()
    return check(args) if args.command == 'check' else make_world(args)


if __name__ == '__main__':
    sys.exit(main())
