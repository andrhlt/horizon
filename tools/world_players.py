#!/usr/bin/env python3
"""Give load-test bots the saved data of a world copy's N most recently active players.

A server benchmark on a copy of a real world is only fair when the bots stand where players stood, in the same
dimensions, with the same inventories: `horizon-bots` log in offline as Bot0, Bot1... and the server loads
`players/data/<offline uuid>.dat` for each. This copies the saved data (position, dimension, inventory,
advancements, statistics) of the N players whose files changed last (the most recently active) to the bots' own
offline UUIDs, in a COPY of the world:

    python3 tools/world_players.py WORLD_COPY 100
    python3 tools/world_players.py WORLD_COPY 100 --extra Tester0=minecraft:overworld --extra Tester1=minecraft:the_end

- `WORLD_COPY` is the world folder (it holds `level.dat` and `players/`). It is changed in place: copy the world
  first (`tools/server_bench.py --world` does, per run). The tool refuses a folder inside this repository's
  tracked tree: player data is private and is never committed, so keep copies outside the checkout, or under
  the ignored `.local/`.
- `--extra NAME=DIMENSION` gives one more offline name the data of the most recent player who was in that
  dimension (a tester who must start in the End, say). The extras choose first, so the bots get the most recent
  players that are left. Repeat it for more.
- `--prefix` names the bots (default `Bot`): Bot0 is the most recent player.
- Which real player went to which bot is written to `bot-sources.json` next to the world copy, not printed: it
  names real players. Only counts per dimension are printed.
- A player's saved data with a damaged file is skipped; the next one takes the place.
"""

import argparse
import gzip
import hashlib
import io
import json
import os
import shutil
import struct
import subprocess
import sys
import uuid
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent


def offline_uuid(name):
    """The UUID an offline-mode server gives a player (UUID.nameUUIDFromBytes("OfflinePlayer:" + name))."""
    h = bytearray(hashlib.md5(('OfflinePlayer:' + name).encode()).digest())
    h[6] = (h[6] & 0x0F) | 0x30
    h[8] = (h[8] & 0x3F) | 0x80
    return uuid.UUID(bytes=bytes(h))


# A small NBT reader and writer that keeps every tag as it was: payloads stay raw bytes, lists and compounds
# are tuples, so a player's file is written back byte for byte apart from the UUID.

def read_payload(f, tag):
    if tag == 1:
        return f.read(1)
    if tag == 2:
        return f.read(2)
    if tag in (3, 5):
        return f.read(4)
    if tag in (4, 6):
        return f.read(8)
    if tag == 7:
        n = f.read(4)
        return n + f.read(struct.unpack('>i', n)[0])
    if tag == 8:
        n = f.read(2)
        return n + f.read(struct.unpack('>H', n)[0])
    if tag == 9:
        element = f.read(1)[0]
        count = struct.unpack('>i', f.read(4))[0]
        return ('list', element, [read_payload(f, element) for _ in range(count)])
    if tag == 10:
        items = []
        while True:
            kind = f.read(1)[0]
            if kind == 0:
                return ('compound', items)
            length = struct.unpack('>H', f.read(2))[0]
            name = f.read(length)
            items.append((kind, name, read_payload(f, kind)))
    if tag == 11:
        n = f.read(4)
        return n + f.read(4 * struct.unpack('>i', n)[0])
    if tag == 12:
        n = f.read(4)
        return n + f.read(8 * struct.unpack('>i', n)[0])
    raise ValueError(f'unknown NBT tag {tag}')


def write_payload(out, tag, value):
    if tag == 9:
        _, element, items = value
        out.write(bytes([element]))
        out.write(struct.pack('>i', len(items)))
        for item in items:
            write_payload(out, element, item)
    elif tag == 10:
        for kind, name, payload in value[1]:
            out.write(bytes([kind]))
            out.write(struct.pack('>H', len(name)))
            out.write(name)
            write_payload(out, kind, payload)
        out.write(b'\0')
    else:
        out.write(value)


def read_nbt(data):
    """(root name, root compound) of a gzip-compressed NBT file's bytes."""
    f = io.BytesIO(gzip.decompress(data))
    tag = f.read(1)[0]
    if tag != 10:
        raise ValueError('the root is not a compound')
    length = struct.unpack('>H', f.read(2))[0]
    name = f.read(length)
    return name, read_payload(f, 10)


def write_nbt(name, root):
    out = io.BytesIO()
    out.write(b'\x0a')
    out.write(struct.pack('>H', len(name)))
    out.write(name)
    write_payload(out, 10, root)
    return gzip.compress(out.getvalue())


def field(root, key):
    for kind, name, payload in root[1]:
        if name == key.encode():
            return kind, payload
    return None, None


def describe(root):
    """((x, y, z), dimension) a player's data says; either may be None."""
    _, pos = field(root, 'Pos')
    _, dim = field(root, 'Dimension')
    xyz = tuple(round(struct.unpack('>d', d)[0]) for d in pos[2]) if pos else None
    return xyz, dim[2:].decode() if dim else None


def with_uuid(root, new):
    """The compound with its UUID replaced by `new` (a uuid.UUID)."""
    ints = struct.unpack('>4i', new.bytes)
    items = [(k, n, p) for k, n, p in root[1] if n != b'UUID']
    items.append((11, b'UUID', struct.pack('>i', 4) + struct.pack('>4i', *ints)))
    return ('compound', items)


def clone(world, source, name):
    """The saved data of the player `source` (a UUID string), given to the offline player `name`. Returns the
    new UUID and (position, dimension). Advancements and statistics are copied as they are."""
    data = world / 'players' / 'data'
    root_name, root = read_nbt((data / f'{source}.dat').read_bytes())
    # Read before anything is written: a damaged file leaves no half-made bot behind.
    described = describe(root)
    new = offline_uuid(name)
    (data / f'{new}.dat').write_bytes(write_nbt(root_name, with_uuid(root, new)))
    for sub in ('advancements', 'stats'):
        saved = world / 'players' / sub / f'{source}.json'
        if saved.exists():
            shutil.copyfile(saved, world / 'players' / sub / f'{new}.json')
    return new, described


def recent_players(world):
    """The UUIDs of the players with saved data, the most recently active first (the data file's change time)."""
    data = world / 'players' / 'data'
    files = [e for e in os.scandir(data) if e.name.endswith('.dat')]
    files.sort(key=lambda e: (e.stat().st_mtime, e.name), reverse=True)
    return [e.name[:-4] for e in files]


def in_repository(path):
    """Whether `path` is inside this repository's work tree and not ignored by git (a place a commit could take
    it from)."""
    path = path.resolve()
    try:
        path.relative_to(ROOT)
    except ValueError:
        return False
    ignored = subprocess.run(['git', 'check-ignore', '-q', str(path)], cwd=ROOT).returncode == 0
    return not ignored


def assign(world, count, prefix='Bot', extras=()):
    """Gives each (NAME, DIMENSION) of `extras` the most recent player in that dimension, then Bot0..Bot{count-1}
    the `count` most recent players left. Returns {name: {source, uuid, dim, pos}}."""
    out = {}
    used = set()
    # A bot's own file from an earlier run is no player to copy.
    mine = {str(offline_uuid(f'{prefix}{i}')) for i in range(count)} | {str(offline_uuid(n)) for n, _ in extras}
    pending = [p for p in recent_players(world) if p not in mine]

    def take(name, wanted=None):
        for source in pending:
            if source in used:
                continue
            try:
                root = read_nbt((world / 'players' / 'data' / f'{source}.dat').read_bytes())[1]
                xyz, dim = describe(root)
            except (OSError, ValueError, EOFError, IndexError, struct.error) as e:
                used.add(source)
                print(f'skip a damaged player file: {type(e).__name__}', file=sys.stderr)
                continue
            if wanted is not None and dim != wanted:
                continue
            used.add(source)
            try:
                new, _ = clone(world, source, name)
            except (OSError, ValueError, EOFError, IndexError, struct.error):
                continue
            out[name] = {'source': source, 'uuid': str(new), 'dim': dim, 'pos': xyz}
            return True
        return False

    # The extras choose first, so a tester can always start in a dimension few players are in.
    for name, wanted in extras:
        if not take(name, wanted):
            raise SystemExit(f'no player left who was in {wanted} for {name}')
    for i in range(count):
        if not take(f'{prefix}{i}'):
            break
    return out


def main():
    parser = argparse.ArgumentParser(description=__doc__.split('\n\n')[0])
    parser.add_argument('world', type=Path, help='a COPY of the world folder (changed in place)')
    parser.add_argument('count', type=int, help='bots: Bot0..BotN-1')
    parser.add_argument('--prefix', default='Bot')
    parser.add_argument('--extra', action='append', default=[], metavar='NAME=DIMENSION',
                        help='one more name that gets the next recent player in that dimension')
    parser.add_argument('--map', type=Path, default=None,
                        help='where the bot to player map goes (default bot-sources.json beside the world)')
    args = parser.parse_args()
    world = args.world.resolve()
    if not (world / 'players' / 'data').is_dir():
        raise SystemExit(f'{world} has no players/data: give the world folder')
    if in_repository(world):
        raise SystemExit(f'{world} is inside the repository: player data is never committed. Use a copy outside it '
                         '(or under the ignored .local/).')
    target = (args.map or world.parent / 'bot-sources.json').resolve()
    if in_repository(target):
        raise SystemExit(f'{target} is inside the repository: the map names real players')
    extras = []
    for item in args.extra:
        name, _, dim = item.partition('=')
        if not name or not dim:
            raise SystemExit(f'--extra {item}: use NAME=DIMENSION')
        extras.append((name, dim))
    result = assign(world, args.count, args.prefix, extras)
    target.write_text(json.dumps(result, indent=1) + '\n')
    os.chmod(target, 0o600)
    per_dimension = {}
    for entry in result.values():
        per_dimension[entry['dim']] = per_dimension.get(entry['dim'], 0) + 1
    print(f'{len(result)} players cloned: {json.dumps(per_dimension, sort_keys=True)}; map in {target}')


if __name__ == '__main__':
    sys.exit(main())
