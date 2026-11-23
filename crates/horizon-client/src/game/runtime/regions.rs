//! Regions: the parts of a level that tick on threads of their own.
//!
//! A level is cut into sections of `4×4` chunks. A section is *ticking* when a chunk in it ticks (or a player
//! stands in it) and a *view* section when it is only inside a player's view distance. Ticking sections
//! [`MERGE_RADIUS`] sections apart or closer are one region; a view section is one region with the sections it
//! touches. A region owns its sections and a ring of one section around each ticking one, so two regions' chunks
//! never meet and a region's ticks find the chunks next to theirs.
//!
//! [`Regions`] decides which region owns what when a level is first split. The rest of the module ticks them
//! (`[regions]`, off by default): a level with players in two or more separate areas is split, and each
//! region's chunks, entities, block entities and players are lent to a pooled region game (`region_pool.rs`)
//! that ticks them on a thread of its own, its players' packets included.
//!
//! A region then keeps its part of the level. Who owns each section is in a [`SectionMap`]
//! (`region_map.rs`) that every region thread reads and claims from. At the end of its own tick a region works
//! out the area its players and ticking chunks need: a region whose sections fall into separate groups splits
//! itself ([`Game::split_off_groups`], the new region takes a spare game from [`Spares`]), and a region that
//! comes within the merge radius of another is merged with it at the next tick boundary, one absorbing the other
//! ([`Game::absorb_region`]). No timer brings a region home: it lands (every part comes home to the server's game)
//! only when the server's thread has something to do with a player or a chunk of theirs (a command, a join or
//! leave, a death, a native mod's hook); one region may come home alone. Terrain is not a reason: a region has a
//! streamer of its own ([`Streaming::fork`]) that loads, generates, lights and unloads the chunks of the
//! sections it owns on its own thread, with its jobs on the pool every streamer shares, and the server's streamer
//! keeps to the chunks no region owns ([`SetShared::server_owns`]). The server's thread waits for them to the region deadline, runs
//! the serial phases on what it still holds, and sums what they report (the night skip's sleep counts, `/tps`'
//! numbers).
use super::region_map::{Area, Claim, Kind, RegionId, SectionMap};
use super::region_profile::Step;
use super::{Dim, Game, Mob, Resources};
use horizon_client::Result;
use horizon_client::join_seq::{JoinList, Joined, ResidentList};
use horizon_core::fast_hash::{FxHashMap, FxHashSet};
use horizon_core::position::ChunkPos;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};

/// A region's game moves to a thread of its own: `Game` has to be `Send` (checked when this compiles).
const _: fn() = || {
    fn send<T: Send>() {}
    send::<Game<'static>>();
};

/// Chunks per side of a region section, as a shift: 2^2 = 4.
pub(super) const SECTION_SHIFT: i32 = 2;
/// Ticking sections this many sections apart or closer are one region.
pub(super) const MERGE_RADIUS: i32 = 2;

/// Which region each loaded chunk belongs to.
pub(super) struct Regions {
    /// The region of each owned section.
    owned: FxHashMap<(i32, i32), usize>,
    /// How many regions the level has.
    pub count: usize,
    /// The sections each region owns (`owned` by region): a split restricted to one region reads only these, not
    /// the level's every chunk.
    by_region: Vec<Vec<(i32, i32)>>,
    /// `owned` as a flat table over the sections' bounds, when that is small enough: every split of the level asks
    /// for each chunk's and entity's region (tens of thousands a tick), one array read each.
    grid: Option<Grid>,
    /// The kind of each section a region owns (`by_region` with its kinds): what a region claims in the
    /// [`SectionMap`] when it is lent.
    kinds: Vec<Area>,
    /// Added to the number of the first part a split makes, so the regions of a split made later keep ranges of
    /// their own for sub-ticks and block entity ids ([`Game::split_level`]).
    band: usize,
    /// While one region is split out alone: the only region that owns anything (`usize::MAX`: all of them).
    /// Read by the thread that splits only.
    only: std::sync::atomic::AtomicUsize,
}

/// The owned sections' regions in one array over their bounding box (`u16::MAX`: no region).
struct Grid {
    x0: i32,
    z0: i32,
    w: i32,
    h: i32,
    cells: Vec<u16>,
}

impl Grid {
    /// Sections in the table at most (4 M cells, 8 MB): players spread wider than that keep the map.
    const MAX_CELLS: i64 = 1 << 22;
    fn of(owned: &FxHashMap<(i32, i32), usize>) -> Option<Self> {
        if owned.is_empty() {
            return None;
        }
        let (mut x0, mut z0, mut x1, mut z1) = (i32::MAX, i32::MAX, i32::MIN, i32::MIN);
        for &(x, z) in owned.keys() {
            x0 = x0.min(x);
            z0 = z0.min(z);
            x1 = x1.max(x);
            z1 = z1.max(z);
        }
        let (w, h) = (
            i64::from(x1) - i64::from(x0) + 1,
            i64::from(z1) - i64::from(z0) + 1,
        );
        if w * h > Self::MAX_CELLS || owned.values().any(|&r| r >= usize::from(u16::MAX)) {
            return None;
        }
        let (w, h) = (w as i32, h as i32);
        let mut cells = vec![u16::MAX; (w * h) as usize];
        for (&(x, z), &r) in owned {
            cells[((z - z0) * w + (x - x0)) as usize] = r as u16;
        }
        Some(Self {
            x0,
            z0,
            w,
            h,
            cells,
        })
    }
    #[inline]
    fn get(&self, x: i32, z: i32) -> Option<usize> {
        let (dx, dz) = (x.wrapping_sub(self.x0), z.wrapping_sub(self.z0));
        if dx < 0 || dz < 0 || dx >= self.w || dz >= self.h {
            return None;
        }
        match self.cells[(dz * self.w + dx) as usize] {
            u16::MAX => None,
            r => Some(usize::from(r)),
        }
    }
}

const TICKING: u8 = 1;
const VIEW: u8 = 2;

impl Regions {
    /// The regions for the chunks that tick (`ticking`) and the players' chunks (`players`), each seeing `view`
    /// chunks around it (`view <= 0`: a player stands in a ticking section and sees nothing of its own).
    ///
    /// Only regions with a player in them are kept when there are players: a region without one gets no thread, its
    /// chunks tick with the rest no region owns.
    pub(super) fn new(
        ticking: impl IntoIterator<Item = (i32, i32)>,
        players: &[(i32, i32)],
        view: i32,
    ) -> Self {
        let mut kind: FxHashMap<(i32, i32), u8> = FxHashMap::default();
        for (x, z) in ticking {
            kind.insert((x >> SECTION_SHIFT, z >> SECTION_SHIFT), TICKING);
        }
        for &(x, z) in players {
            if view <= 0 {
                kind.insert((x >> SECTION_SHIFT, z >> SECTION_SHIFT), TICKING);
                continue;
            }
            for sx in (x - view) >> SECTION_SHIFT..=(x + view) >> SECTION_SHIFT {
                for sz in (z - view) >> SECTION_SHIFT..=(z + view) >> SECTION_SHIFT {
                    kind.entry((sx, sz)).or_insert(VIEW);
                }
            }
        }
        // The same numbering every tick for the same sections: regions in the order of their first section.
        let mut keys: Vec<(i32, i32)> = kind.keys().copied().collect();
        keys.sort_unstable();
        let mut region: FxHashMap<(i32, i32), usize> = FxHashMap::default();
        let mut count = 0;
        for start in keys {
            if region.contains_key(&start) {
                continue;
            }
            region.insert(start, count);
            let mut stack = vec![start];
            while let Some((x, z)) = stack.pop() {
                let mine = kind[&(x, z)];
                let reach = if mine == TICKING { MERGE_RADIUS } else { 1 };
                for dx in -reach..=reach {
                    for dz in -reach..=reach {
                        let next = (x + dx, z + dz);
                        let Some(&theirs) = kind.get(&next) else {
                            continue;
                        };
                        if region.contains_key(&next) {
                            continue;
                        }
                        let apart = dx.abs().max(dz.abs());
                        let joins = if mine == TICKING && theirs == TICKING {
                            apart <= MERGE_RADIUS
                        } else {
                            apart <= 1
                        };
                        if joins {
                            region.insert(next, count);
                            stack.push(next);
                        }
                    }
                }
            }
            count += 1;
        }
        if !players.is_empty() {
            let mut has_player = vec![false; count];
            for &(x, z) in players {
                if let Some(&r) = region.get(&(x >> SECTION_SHIFT, z >> SECTION_SHIFT)) {
                    has_player[r] = true;
                }
            }
            if has_player.iter().any(|p| !p) {
                let mut renumber = vec![usize::MAX; count];
                let mut kept = 0;
                for (r, _) in has_player.iter().enumerate().filter(|(_, p)| **p) {
                    renumber[r] = kept;
                    kept += 1;
                }
                region.retain(|_, r| renumber[*r] != usize::MAX);
                for r in region.values_mut() {
                    *r = renumber[*r];
                }
                count = kept;
            }
        }
        let mut owned = FxHashMap::default();
        for (&(x, z), &r) in &region {
            if kind[&(x, z)] == TICKING {
                for dx in -1..=1 {
                    for dz in -1..=1 {
                        owned.insert((x + dx, z + dz), r);
                    }
                }
            }
        }
        for (&key, &r) in &region {
            owned.insert(key, r);
        }
        let grid = Grid::of(&owned);
        let mut by_region: Vec<Vec<(i32, i32)>> = vec![Vec::new(); count];
        let mut kinds: Vec<Area> = vec![Area::default(); count];
        for (&key, &r) in &owned {
            by_region[r].push(key);
            kinds[r].insert(
                key,
                match kind.get(&key) {
                    Some(&TICKING) => Kind::Ticking,
                    Some(_) => Kind::View,
                    None => Kind::Ring,
                },
            );
        }
        Self {
            owned,
            count,
            by_region,
            grid,
            kinds,
            band: 0,
            only: std::sync::atomic::AtomicUsize::new(usize::MAX),
        }
    }

    /// The one region that owns `area`, split out alone: the layout of a region that is lent again, or carved out
    /// of another (`band`: see [`Regions::band`]).
    pub(super) fn for_area(area: &Area, band: usize) -> Self {
        let owned: FxHashMap<(i32, i32), usize> = area.keys().map(|&s| (s, 0)).collect();
        Self {
            by_region: vec![area.keys().copied().collect()],
            kinds: vec![area.clone()],
            owned,
            count: 1,
            grid: None,
            band,
            only: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// The sections region `k` owns, with their kinds.
    pub(super) fn area_of(&self, k: usize) -> Area {
        self.kinds.get(k).cloned().unwrap_or_default()
    }

    /// The region a split is restricted to, if it is ([`Regions::for_area`]).
    fn only_region(&self) -> Option<usize> {
        match self.only.load(Ordering::Relaxed) {
            usize::MAX => None,
            k => Some(k),
        }
    }

    /// How many parts a split makes: one while it is restricted to a region (its chunks are number 0), else every
    /// region's.
    pub(super) fn parts(&self) -> usize {
        if self.only_region().is_some() {
            1
        } else {
            self.count
        }
    }

    /// Every chunk position in the sections region `k` owns: all the chunks it can hold, loaded or not.
    pub(super) fn chunk_keys(&self, k: usize) -> Vec<(i32, i32)> {
        let side = 1 << SECTION_SHIFT;
        let mut keys =
            Vec::with_capacity(self.by_region.get(k).map_or(0, Vec::len) << (2 * SECTION_SHIFT));
        for &(sx, sz) in self.by_region.get(k).into_iter().flatten() {
            for dx in 0..side {
                for dz in 0..side {
                    keys.push(((sx << SECTION_SHIFT) + dx, (sz << SECTION_SHIFT) + dz));
                }
            }
        }
        keys
    }

    /// The region that owns chunk `(x, z)`.
    #[inline]
    pub(super) fn of_chunk(&self, (x, z): (i32, i32)) -> Option<usize> {
        let (sx, sz) = (x >> SECTION_SHIFT, z >> SECTION_SHIFT);
        let r = match &self.grid {
            Some(g) => g.get(sx, sz),
            None => self.owned.get(&(sx, sz)).copied(),
        };
        match self.only.load(Ordering::Relaxed) {
            usize::MAX => r,
            k => r.filter(|&r| r == k).map(|_| 0),
        }
    }
}

/// What a game keeps for ticking by regions: whether it is a region's game, its handle on the resources that
/// outlives the server's borrow, the end of its block of entity ids, and the games made for regions and kept.
#[derive(Default)]
pub(super) struct RegionState {
    /// This game is a region's: it has no level, players or connections of its own, they are lent in for a tick.
    pub worker: bool,
    /// The resources as a region game holds them, made once and kept for the server's life.
    static_res: Option<&'static Arc<Resources>>,
    /// The end of the block of entity ids this game hands out (a region game's, [`Game::refill_entity_ids`]).
    pub entity_id_end: u32,
    /// The server's shared counter the blocks of entity ids come from.
    pub entity_pool: Option<Arc<AtomicU32>>,
    /// Region games made and not in use: the server's thread stocks them, region threads take one to split off a
    /// region of their own.
    pub spares: Arc<Spares>,
    /// The ids of the regions made so far, shared by every region ([`RegionId`]).
    pub(super) ids: Arc<AtomicU64>,
    /// Region games being made on threads of their own (making one reads files and builds a whole game: a stall
    /// of seconds on the server's thread), taken in once made.
    factory: Vec<std::sync::mpsc::Receiver<std::result::Result<Box<Game<'static>>, String>>>,
    /// Each level's resident regions: their games keep their parts of the level and their players.
    pub resident: std::collections::HashMap<Dim, ResidentSet>,
    /// A level's resident regions started before its server-side work, waited for by its world tick.
    flight: Option<(Dim, Flight)>,
    /// Ticks left before the level may be split again after a region failed.
    paused: u32,
    /// How many entities the level held (here and in resident regions) when the save began, before the regions
    /// landed: [`Game::save_entities`] warns when it then writes none.
    pub(super) entities_before_save: Option<usize>,
    /// What native mods sent to players who are away in a region (a clientbound packet, whether it is for a
    /// Horizon client only): delivered to the region's player before its next tick.
    pub(super) away_out: Vec<(u128, horizon_protocol::packets::play::Clientbound, bool)>,
    /// Where the server thread's tick time goes while regions are resident.
    pub(super) prof: super::region_profile::TickProfile,
    /// A region game's own part: where it stands among its level's regions.
    pub(super) link: Option<RegionLink>,
    /// The sections this region game owns, as it last claimed them in the level's [`SectionMap`].
    pub(super) area: Area,
    /// What the area was worked out from last (players' places, ticking chunks), so a tick where nothing moved
    /// skips the work.
    pub(super) topology_seen: Option<u64>,
    /// The task queues and hand-offs every game of the server shares (game/runtime/region_tasks.rs).
    pub(super) tasks: Arc<super::region_tasks::Tasks>,
    /// What this game asked other regions and waits for, by the player asked about.
    pub(super) questions: HashMap<u128, super::region_tasks::Question>,
    /// The command now running may be held for an answer ([`Game::defer_command`]).
    pub(super) deferrable: bool,
    /// The command that just ran asked for an answer and is held.
    pub(super) command_deferred: bool,
    /// A player came to or left this region game during its tick (a hand-off): the server's thread is told its
    /// connections.
    pub(super) conns_dirty: bool,
    /// The tick a command run again first ran in, while it is.
    pub(super) deferred_since: Option<u64>,
    /// The global tick every game of the server shares: the snapshots a region reads, the messages it sends
    /// (game/runtime/global_tick.rs).
    pub(super) global: Arc<super::global_tick::GlobalTick>,
    /// What only the global tick keeps: the censuses it was sent and the changes it applied. The server's own
    /// game holds it.
    pub(super) gstate: super::global_tick::GlobalState,
    /// This region's changes to the level that the snapshot does not hold yet, by their message number.
    pub(super) unacked: Vec<(u64, super::global_tick::Change)>,
    /// Messages this region sent that change the level.
    pub(super) change_seq: u64,
    /// The server tick of the snapshot this region read last, and the level data it took from it.
    pub(super) global_seen: Option<u64>,
    pub(super) level_seen: Option<Arc<horizon_client::save::Level>>,
    /// What vanilla's mob caps counted in this game's own chunks in its last spawning pass: spawnable chunks and
    /// mobs per category, sent to the global tick as a census.
    pub(super) last_caps: Option<(i32, [i32; 8])>,
}

/// Region games made and not in use.
#[derive(Default)]
pub(super) struct Spares {
    games: std::sync::Mutex<Vec<Box<Game<'static>>>>,
    /// How many more a region thread found missing when it wanted to split: the server's thread makes them.
    wanted: AtomicUsize,
}

impl Spares {
    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Box<Game<'static>>>> {
        self.games
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
    /// A game for a region that splits off.
    pub(super) fn take(&self) -> Option<Box<Game<'static>>> {
        let game = self.lock().pop();
        if game.is_none() {
            self.wanted.fetch_add(1, Ordering::Relaxed);
        }
        game
    }
    pub(super) fn give(&self, mut game: Box<Game<'static>>) {
        // Its streamer rests until the game is lent again (a streamer of the next level's is made then).
        game.streaming.own = None;
        game.streaming.paused = true;
        game.regions.link = None;
        game.regions.area.clear();
        game.regions.topology_seen = None;
        game.regions.global_seen = None;
        game.regions.level_seen = None;
        game.regions.unacked.clear();
        self.lock().push(game);
    }
    fn len(&self) -> usize {
        self.lock().len()
    }
}

/// Where a resident region stands among its level's: what its own thread needs to claim sections, split and ask
/// for a merge without the server's thread.
#[derive(Clone)]
pub(super) struct RegionLink {
    pub(super) shared: Arc<SetShared>,
    pub(super) id: RegionId,
    pub(super) spares: Arc<Spares>,
    pub(super) ids: Arc<AtomicU64>,
    pub(super) pool: Arc<AtomicU32>,
}

/// What one level's resident regions share.
#[derive(Default)]
pub(super) struct SetShared {
    /// Who owns each section.
    pub(super) map: SectionMap,
    /// Pairs of regions within the merge radius of each other (the smaller id first), found by their own threads:
    /// the server's thread merges them at the next tick boundary.
    pub(super) merges: std::sync::Mutex<std::collections::BTreeSet<(RegionId, RegionId)>>,
    /// The regions that are home (landed) this tick: their chunks are the server's again.
    landed: std::sync::RwLock<FxHashSet<RegionId>>,
}

impl SetShared {
    /// Whether the server's own streaming may hold chunk `(x, z)`: no resident region owns its section.
    pub(super) fn server_owns(&self, chunk: (i32, i32)) -> bool {
        match self.map.owner_of_chunk(chunk) {
            None => true,
            Some(o) => self
                .landed
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .contains(&o.id),
        }
    }
    /// Notes which regions are home this tick.
    pub(super) fn set_landed(&self, ids: impl IntoIterator<Item = RegionId>) {
        let mut landed = self
            .landed
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        landed.clear();
        landed.extend(ids);
    }
    pub(super) fn ask_merge(&self, a: RegionId, b: RegionId) {
        if a != b {
            self.merges
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert((a.min(b), a.max(b)));
        }
    }
}

/// A level's regions that stay on their threads between ticks.
pub(super) struct ResidentSet {
    pub(super) games: Vec<Slot>,
    /// All of the regions' chunks, for the streamer: lent columns are loaded, only not in the server's world.
    pub(super) lent: Arc<horizon_client::fast_hash::FxHashSet<(i32, i32)>>,
    /// The sections' owners.
    pub(super) shared: Arc<SetShared>,
}

/// Resident regions started on their threads this tick, not yet waited for.
struct Flight {
    set: ResidentSet,
    /// The regions started this tick.
    launched: Vec<usize>,
    failed: Option<String>,
    /// When they started: the deadline counts from here.
    started: std::time::Instant,
}

/// What a region's tick sends back with its game.
pub(super) struct Done {
    game: Box<Game<'static>>,
    result: std::result::Result<(), String>,
    took: f32,
    report: Report,
}

/// What a region found at the end of its tick, for the server's thread.
#[derive(Default)]
struct Report {
    /// Regions it split off.
    born: Vec<Born>,
    /// The connections of its players after that, when it split.
    conns: Option<Vec<u32>>,
    /// Why it has to come home alone: it has no player left. (An area that grew or shrank is no reason: the
    /// region loads the chunks of its new sections itself and unloads those of the sections it gave up.)
    wants: Option<&'static str>,
}

/// A region a region split off: its game with its players, and its id in the map.
pub(super) struct Born {
    game: Box<Game<'static>>,
    id: RegionId,
    conns: Vec<u32>,
}

/// One resident region: its game when it is home, or the channel its thread sends it back on while it ticks.
pub(super) struct Slot {
    pub(super) game: Option<Box<Game<'static>>>,
    pub(super) running: Option<std::sync::mpsc::Receiver<Done>>,
    /// Where its players stood when it was last home: the areas the server keeps loaded for them.
    pub(super) places: Vec<(i32, i32)>,
    /// When its tick started, and when the watchdog last told of it.
    pub(super) since: std::time::Instant,
    pub(super) told: Option<std::time::Instant>,
    /// How long its last tick took (ms) and how many chunks it held then.
    pub(super) took: f32,
    pub(super) chunks: usize,
    /// Its part of the level and its players are home, ticking with the server's thread this tick; it is lent
    /// again at the start of the next.
    pub(super) landed: bool,
    /// Lent again: its game takes the tick's settings, as one just lent.
    pub(super) fresh: bool,
    /// The connections of its players.
    pub(super) conns: Vec<u32>,
    /// Its players as native mods see them (`native_server_players`), as of the last time the region was home.
    pub(super) players: Vec<AwayPlayer>,
    /// Its players' sleep counts as it last reported them.
    pub(super) sleep: (i64, i64, i64),
    /// Its id in the level's [`SectionMap`], and the name it has among the regions ([`Game::route_region_chat`]).
    pub(super) uid: RegionId,
    /// Chat lines said elsewhere for its players, sent when it is next lent or lands.
    pub(super) mail: Vec<horizon_client::lan::hub::chat::RegionChat>,
    /// Why its own thread asked for it to come home ([`Report::wants`]).
    pub(super) wants: Option<&'static str>,
    /// The regions it split off, until the set takes them in.
    pub(super) born: Vec<Born>,
    /// Its players as `server-status` tells of them, and what it held, as of the last time the region was home.
    pub(super) status: Vec<AwayStatus>,
    pub(super) entities: usize,
}

/// A player away in a region, as `server-status` reports them: the supervisor asks every second, and a region
/// that had to come home for each answer would never stay out.
pub(super) struct AwayStatus {
    pub name: String,
    pub uuid: u128,
    pub address: std::net::SocketAddr,
    pub latency: Arc<std::sync::atomic::AtomicI32>,
    pub dimension: String,
    pub position: [f64; 3],
}

pub(super) struct AwayPlayer {
    pub conn: u32,
    pub id: u32,
    pub uuid: u128,
    pub sender: horizon_client::lan::PlayerSender,
}

impl Slot {
    pub(super) fn new(game: Box<Game<'static>>, uid: RegionId) -> Self {
        Self {
            uid,
            mail: Vec::new(),
            game: Some(game),
            running: None,
            places: Vec::new(),
            since: std::time::Instant::now(),
            told: None,
            took: 0.0,
            chunks: 0,
            landed: false,
            fresh: false,
            conns: Vec::new(),
            players: Vec::new(),
            sleep: (0, 0, 0),
            wants: None,
            born: Vec::new(),
            status: Vec::new(),
            entities: 0,
        }
    }
    /// Takes the game in if its thread finished (waiting until `until`, or for good when None); false while it
    /// still runs.
    pub(super) fn home(
        &mut self,
        until: Option<std::time::Instant>,
    ) -> std::result::Result<bool, String> {
        let Some(rx) = self.running.as_ref() else {
            return Ok(true);
        };
        let got = match until {
            None => rx
                .recv()
                .map_err(|_| std::sync::mpsc::TryRecvError::Disconnected),
            Some(t) => {
                match rx.recv_timeout(t.saturating_duration_since(std::time::Instant::now())) {
                    Ok(b) => Ok(b),
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                        Err(std::sync::mpsc::TryRecvError::Empty)
                    }
                    Err(_) => Err(std::sync::mpsc::TryRecvError::Disconnected),
                }
            }
        };
        match got {
            Ok(Done {
                game,
                result,
                took,
                report,
            }) => {
                self.running = None;
                self.took = took;
                self.chunks = game.world.chunks.len();
                self.places = game
                    .lan
                    .as_ref()
                    .map(|h| h.hub.guests.iter().map(|g| g.chunk()).collect())
                    .unwrap_or_default();
                if let Some(conns) = report.conns {
                    self.conns = conns;
                }
                if report.wants.is_some() {
                    self.wants = report.wants;
                }
                self.born.extend(report.born);
                self.game = Some(game);
                result.map(|()| true)
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => Ok(false),
            Err(_) => {
                self.running = None;
                Err("a region thread stopped".to_owned())
            }
        }
    }
}

impl ResidentSet {
    /// The regions the slots' threads split off since, as slots of their own.
    fn take_in_born(&mut self) {
        let mut born = Vec::new();
        for slot in &mut self.games {
            born.append(&mut slot.born);
        }
        for b in born {
            let mut slot = Slot::new(b.game, b.id);
            slot.conns = b.conns;
            slot.fresh = true;
            self.games.push(slot);
        }
    }
}

/// Entity ids a region game takes from the one global counter at a time, and how few it may have left before a
/// tick before it takes more (no tick hands out anywhere near that many). The counter only goes up, as vanilla's
/// `Entity.nextEntityId` does: a block is never given back, so what a game leaves of one is lost, at most
/// `ID_REFILL` of a block.
const ID_BLOCK: u32 = 1 << 16;
const ID_REFILL: u32 = 1 << 14;
/// Ticks a level stays unsplit after one of its regions failed.
const FAILED_PAUSE: u32 = 200;

/// Sections a region may hold that its players and ticking chunks no longer need before it gives them up: the
/// area follows the players in steps, not at every section they cross.
const SHRINK_SLACK: usize = 24;

/// Region games kept ready for a region to split off into.
const SPARE_GAMES: usize = 1;

/// How long a tick waits for its regions (`HORIZON_REGION_DEADLINE_MS`, 50): one that runs longer keeps going
/// on its own thread and its players are ticked again once it is home, so a slow region does not hold the others.
fn region_deadline() -> std::time::Duration {
    static MS: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    std::time::Duration::from_millis(*MS.get_or_init(|| {
        std::env::var("HORIZON_REGION_DEADLINE_MS")
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(50)
    }))
}

/// Which resident regions have to come home: all of them, or those named (with the reason).
enum Land {
    All(&'static str),
    Slots(Vec<(usize, &'static str)>),
}

/// Whether region game `w` has something for the server's thread, so its region must come back: commands, menus
/// of native mods, a player dying or leaving, a raid (`region_raids_want_home`). A player who stays dead costs one
/// landing, for the death: while they wait on the death screen their region stays out, and comes home for what
/// they send (their respawn is a packet).
fn wants_land(w: &Game<'_>) -> bool {
    if !w.level_teleports.is_empty() || !w.server_trips.is_empty() || w.region_raids_want_home() {
        return true;
    }
    let Some(h) = w.lan.as_ref() else {
        return true;
    };
    !h.commands.is_empty()
        || !h.quiet.is_empty()
        || !h.hub.native_menu_events.is_empty()
        || h.hub
            .guests
            .iter()
            .any(|g| ((g.dead || g.health <= 0.0) && !g.death_landed) || !g.left_reason.is_empty())
}

impl Game<'_> {
    /// `n` more region games made on threads of their own.
    fn request_region_games(&mut self, n: usize) {
        let res = self.static_resources();
        for _ in 0..n {
            let (tx, rx) = std::sync::mpsc::channel();
            let (pack, options, settings) = (
                self.pack.clone(),
                self.options.clone(),
                self.settings.clone(),
            );
            let spawned = std::thread::Builder::new()
                .name("Region game maker".into())
                .spawn(move || {
                    let made = super::integrated::server_game(res, pack, options, settings)
                        .map(Box::new)
                        .map_err(|e| e.to_string());
                    let _ = tx.send(made);
                });
            if spawned.is_ok() {
                self.regions.factory.push(rx);
            }
        }
    }

    /// Keeps [`SPARE_GAMES`] ready, and as many more as region threads found missing: a region splits itself at
    /// the end of its tick and takes a game for the new region from them.
    fn stock_spares(&mut self) {
        self.collect_region_games();
        let wanted = self.regions.spares.wanted.swap(0, Ordering::Relaxed);
        let have = self.regions.spares.len() + self.regions.factory.len();
        let want = (SPARE_GAMES + wanted).min(4);
        if have < want {
            self.request_region_games(want - have);
        }
    }

    /// The region games made since, set up for the level in place and kept.
    fn collect_region_games(&mut self) {
        let mut i = 0;
        while i < self.regions.factory.len() {
            match self.regions.factory[i].try_recv() {
                Ok(made) => {
                    self.regions.factory.remove(i);
                    match made {
                        Ok(mut w) => {
                            w.copy_world_settings_from(self);
                            w.regions.worker = true;
                            w.next_entity = 0;
                            w.regions.entity_id_end = 0;
                            self.regions.spares.give(w);
                        }
                        Err(e) => log::warn!("a region game could not be made: {e}"),
                    }
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => i += 1,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.regions.factory.remove(i);
                }
            }
        }
    }

    /// The resources as a region game holds them: its own handle, made once and kept for the server's life.
    fn static_resources(&mut self) -> &'static Arc<Resources> {
        match self.regions.static_res {
            Some(r) => r,
            None => {
                let r: &'static Arc<Resources> = Box::leak(Box::new(Arc::clone(&self.res)));
                self.regions.static_res = Some(r);
                r
            }
        }
    }

    /// What every region game shares with the server's: the world's settings and tables, as they are now
    /// (copied again when they change: `/gamerule`, `/difficulty`, `/reload`).
    pub(super) fn copy_world_settings_from(&mut self, from: &Game<'_>) {
        self.level.clone_from(&from.level);
        self.level_shapes = from.level_shapes;
        self.world_root.clone_from(&from.world_root);
        // The level's save folder: the region saves the chunks and entities it unloads itself.
        self.save = Arc::clone(&from.save);
        self.world_biomes = from.world_biomes.clone();
        self.end.set_locator(from.end.locator());
        self.dim = from.dim;
        // The task queues are the server's, one set for all its games, and so is the global tick.
        self.regions.tasks = Arc::clone(&from.regions.tasks);
        self.regions.global = Arc::clone(&from.regions.global);
        self.world_flat = from.world_flat;
        self.world_debug = from.world_debug;
        // The server's tables, shared: building them reads every item and recipe file from disk.
        self.world_tables = from.world_tables.clone();
        self.server_loot = from.server_loot.clone();
        self.server_items.clone_from(&from.server_items);
        // The ban, op and whitelist lists, shared: a ban the anticheat makes on a region's thread is the server's.
        self.server_lists.clone_from(&from.server_lists);
        self.share_spawn_conditions();
        self.copy_tick_settings_from(from);
        self.lend_scoreboard_from(from);
    }

    /// What can change from tick to tick that a region reads but never writes: the rules, the difficulty, the
    /// clock, the border, the ticket levels.
    pub(super) fn copy_tick_settings_from(&mut self, from: &Game<'_>) {
        self.copy_clock_from(from);
        self.tickets = from.tickets.clone();
    }

    /// [`Self::copy_tick_settings_from`] but the ticket levels, which a resident region keeps for its own players.
    pub(super) fn copy_clock_from(&mut self, from: &Game<'_>) {
        // Every region, every tick, on the server's thread: what rarely changes (the rules are a map, the
        // border a shape) is compared, and copied only when it differs; the clock is a few hundred bytes.
        if self.ui.rules != from.ui.rules {
            self.ui.rules.clone_from(&from.ui.rules);
        }
        self.ui.difficulty = from.ui.difficulty;
        self.clock.clone_from(&from.clock);
        self.ticks = from.ticks;
        if self.border != from.border {
            self.border.clone_from(&from.border);
        }
        self.level.difficulty_locked = from.level.difficulty_locked;
    }

    /// The next entity ids this region game hands out: a block of its own from `pool` (the one global counter)
    /// when the one it has runs low, so no two threads ever give out the same id. An id is handed out once in
    /// the server's life, so nothing remembered under an id in a fresh block can belong to an earlier entity.
    pub(super) fn refill_entity_ids(&mut self, pool: &AtomicU32) {
        refill_id_block(pool, &mut self.next_entity, &mut self.regions.entity_id_end);
    }
}

/// Takes a new block of ids from `pool` into (`next`, `end`) when fewer than [`ID_REFILL`] are left in the
/// current one. The counter saturates below `u32::MAX` instead of wrapping onto ids in use, so a world that
/// really runs out of ids says so (`Game::entity_ids_exhausted`) and spawns nothing.
fn refill_id_block(pool: &AtomicU32, next: &mut u32, end: &mut u32) {
    if end.saturating_sub(*next) >= ID_REFILL {
        return;
    }
    let start = pool
        .try_update(Ordering::Relaxed, Ordering::Relaxed, |at| {
            Some(at.saturating_add(ID_BLOCK))
        })
        .unwrap_or(u32::MAX);
    *next = start;
    *end = start.saturating_add(ID_BLOCK);
}

impl Game<'_> {
    /// Makes this game's streamer the region's `link` stands for: it holds the chunks of the sections the region
    /// owns in the level's map and no others, so a chunk is never finished in two games, and it runs.
    pub(super) fn own_region_streaming(&mut self, link: &RegionLink) {
        let (shared, id) = (Arc::clone(&link.shared), link.id);
        self.streaming.own = Some(Arc::new(move |x, z| {
            shared
                .map
                .owner_of_chunk((x, z))
                .is_some_and(|o| o.id == id)
        }));
        self.streaming.paused = false;
        self.streaming.remote = false;
    }

    /// Region `part` of the level in place, moved into region game `w`: its chunks with their block entities,
    /// block and fluid ticks and random-tick caches, every entity that stands in it, and what is kept by block or
    /// by entity in it.
    pub(super) fn lend_region(&mut self, w: &mut Game<'_>, mut part: RegionPart) {
        // The region streams its own chunks (it loads, generates, lights and unloads them on its thread): its
        // streamer is one more of the level's, with the same generator and workers, and takes in what the lender's
        // had for the chunks that move. A game that was a region of this level before keeps its streamer.
        if w.regions.link.is_none() {
            w.streaming = self.streaming.fork();
        }
        let unlit = self
            .streaming
            .release_columns(part.world.chunks.keys().copied());
        w.streaming.adopt_columns(part.world.chunks.values());
        part.world.dirty_light.extend(unlit);
        w.world = part.world;
        w.block_ticks = part.ticks;
        w.random_tick_sections = part.sections;
        w.rand_sequences = part.sequences;
        w.mobs.extend(part.mobs);
        w.creaking.lend_scanned(part.scanned);
        w.dropped.extend(part.dropped);
        w.orbs.extend(part.orbs);
        w.creatures.projectiles.extend(part.projectiles);
        w.falling.extend(part.falling);
        w.paintings.extend(part.paintings);
        w.item_frames.extend(part.item_frames);
        w.armor_stands.extend(part.armor_stands);
        w.cushions.extend(part.cushions);
        w.others.append(&mut part.others.into_iter().collect());
        w.boats.extend(part.boats);
        w.carts.extend(part.carts);
        w.bolts = part.bolts;
        w.crystals.extend(part.crystals);
        w.breath_clouds
            .append(&mut part.breath_clouds.into_iter().collect());
        w.tridents.extend(part.tridents);
        w.rockets.extend(part.rockets);
        w.ominous_spawners
            .append(&mut part.ominous_spawners.into_iter().collect());
        w.tnts.extend(part.tnts);
        w.hoppers = part.hoppers;
        w.jukebox_sounds = part.jukebox_sounds;
        w.lid_openers = part.lid_openers;
        w.knot_ids = part.knot_ids;
        w.lectern_pulses = part.lectern_pulses;
        let a = &mut w.be_anims;
        a.chests = std::mem::take(&mut part.anims.chests);
        a.books = std::mem::take(&mut part.anims.books);
        a.shulkers = std::mem::take(&mut part.anims.shulkers);
        a.bells = std::mem::take(&mut part.anims.bells);
        a.conduits = std::mem::take(&mut part.anims.conduits);
        a.skulls = std::mem::take(&mut part.anims.skulls);
        a.pots = std::mem::take(&mut part.anims.pots);
        a.gateway_servers = std::mem::take(&mut part.anims.gateway_servers);
        a.conduit_servers = std::mem::take(&mut part.anims.conduit_servers);
        // The ids of the region's entities: what is remembered of each goes with them.
        let mut ids: Vec<u32> = w.mobs.iter().map(|m| m.id).collect();
        ids.extend(w.dropped.iter().map(|d| d.id));
        ids.extend(w.boats.iter().map(|b| b.id));
        ids.extend(w.carts.iter().map(|c| c.id));
        ids.extend(w.armor_stands.iter().map(|s| s.id));
        ids.extend(w.orbs.iter().map(|o| o.id));
        ids.extend(w.creatures.projectiles.iter().map(|p| p.id));
        ids.extend(w.falling.iter().map(|f| f.id));
        ids.extend(w.tnts.iter().map(|t| t.id));
        ids.extend(w.tridents.iter().map(|t| t.id));
        ids.extend(w.rockets.iter().map(|r| r.id));
        ids.extend(w.crystals.iter().map(|c| c.id));
        ids.extend(w.bolts.iter().map(|b| b.id));
        ids.extend(w.ominous_spawners.iter().map(|o| o.id));
        ids.extend(w.breath_clouds.iter().map(|b| b.id));
        ids.extend(w.paintings.iter().map(|p| p.id));
        ids.extend(w.item_frames.iter().map(|f| f.id));
        ids.extend(w.cushions.iter().map(|c| c.id));
        ids.extend(w.others.iter().map(|o| o.id));
        for id in ids {
            if let Some(attached) = self.entity_attachments.remove(&id) {
                w.entity_attachments.insert(id, attached);
            }
            if let Some(u) = self.persist.uuids.remove(&id) {
                w.persist.uuids.insert(id, u);
            }
            if let Some(t) = self.persist.raw.remove(&id) {
                w.persist.raw.insert(id, t);
            }
            if let Some(slots) = self.persist.passenger_slots.remove(&id) {
                w.persist.passenger_slots.insert(id, slots);
            }
            if self.died.remove(&id) {
                w.died.insert(id);
            }
            if let Some(v) = self.love_cause.remove(&id) {
                w.love_cause.insert(id, v);
            }
            if let Some(v) = self.last_hurt_by_player.remove(&id) {
                w.last_hurt_by_player.insert(id, v);
            }
            // Who led it to its death: the server's lead tick would drop it for a mob it does not hold.
            if let Some(v) = self.lead_killers.remove(&id) {
                w.lead_killers.insert(id, v);
            }
        }
        w.lend_raids_from(self);
        w.world.game_time = self.world.game_time;
        w.world.day_time = self.world.day_time;
        w.world.weather = self.world.weather;
        w.world.health_rules = self.world.health_rules;
        w.world.watched = self.world.watched.clone();
    }

    /// The players this game's host holds, and their fishing hooks, bound to this game's own entity table: after
    /// a region's were lent to it, or came home. A lent player left bound to the server's table had its turns on
    /// the region's thread take it out of the server's table and put it back there, and a server tick that ended
    /// in between found it still out and stopped the server (`UnbalancedPass`).
    pub(super) fn bind_lan_residents(&mut self) {
        let journal = self.resident_entities.table.borrow().producer();
        self.bind_specialized_resident_owners(&journal);
    }

    /// Region game `w`'s part of the level back in place (after the level's own data, in region order), with
    /// what its tick left for the server: level events, spills, sounds and particles for the network.
    pub(super) fn take_region_back(
        &mut self,
        w: &mut Game<'_>,
        chunks: &[ChunkPos],
    ) -> horizon_client::random_sequences::RandomSequences {
        // What the region remembers of entities that died there dies with them, before it is handed back: a
        // stale record would outlive its id and be inherited by whichever entity reuses it.
        w.prune_entity_memory();
        self.merge_region_scores(w);
        self.take_maps_back(w);
        self.take_raids_back(w);
        let stand_in = self.world.stand_in();
        let mut region_world = std::mem::replace(&mut w.world, stand_in);
        // Entities the region's chunks loaded that it has not added yet go to the server's list: its next
        // spawn step adds them, into the chunks that are home now.
        let waiting = std::mem::take(&mut region_world.loaded_entities);
        self.world.loaded_entities.extend(waiting);
        let region_ticks = std::mem::take(&mut w.block_ticks);
        let region_sections = std::mem::take(&mut w.random_tick_sections);
        let world = &mut self.world;
        std::thread::scope(|scope| {
            let back = scope.spawn(move || world.take_back(region_world));
            self.block_ticks.take_back(region_ticks, chunks);
            self.random_tick_sections.take_back(region_sections);
            if let Err(e) = back.join() {
                std::panic::resume_unwind(e);
            }
        });
        // The chunks are this game's streamer's now, as they were its lender's before.
        let unlit = w
            .streaming
            .release_columns(chunks.iter().map(|c| (c.x, c.z)));
        self.world.dirty_light.extend(unlit);
        self.streaming.adopt_columns(
            chunks
                .iter()
                .filter_map(|c| self.world.chunks.get(&(c.x, c.z))),
        );
        self.mobs.append(&mut w.mobs);
        self.creaking.take_scanned_back(&mut w.creaking);
        self.dropped.append(&mut w.dropped);
        self.orbs.append(&mut w.orbs);
        self.creatures
            .projectiles
            .append(&mut w.creatures.projectiles);
        self.falling.append(&mut w.falling);
        self.paintings.append(&mut w.paintings);
        self.item_frames.append(&mut w.item_frames);
        self.armor_stands.append(&mut w.armor_stands);
        self.cushions.append(&mut w.cushions);
        self.others.append(&mut w.others);
        self.boats.append(&mut w.boats);
        self.carts.append(&mut w.carts);
        self.bolts.append(&mut w.bolts);
        self.crystals.append(&mut w.crystals);
        self.breath_clouds.append(&mut w.breath_clouds);
        self.tridents.append(&mut w.tridents);
        self.rockets.append(&mut w.rockets);
        self.ominous_spawners.append(&mut w.ominous_spawners);
        self.tnts.append(&mut w.tnts);
        self.hoppers.extend(w.hoppers.drain());
        self.jukebox_sounds.extend(w.jukebox_sounds.drain());
        self.lid_openers.extend(w.lid_openers.drain());
        self.knot_ids.extend(w.knot_ids.drain());
        self.lectern_pulses.append(&mut w.lectern_pulses);
        let (a, b) = (&mut self.be_anims, &mut w.be_anims);
        a.chests.extend(b.chests.drain());
        a.books.extend(b.books.drain());
        a.shulkers.extend(b.shulkers.drain());
        a.bells.extend(b.bells.drain());
        a.conduits.extend(b.conduits.drain());
        a.skulls.extend(b.skulls.drain());
        a.pots.extend(b.pots.drain());
        a.gateway_servers.extend(b.gateway_servers.drain());
        a.conduit_servers.extend(b.conduit_servers.drain());
        self.persist.uuids.append(&mut w.persist.uuids);
        self.persist.raw.extend(w.persist.raw.drain());
        self.persist
            .passenger_slots
            .extend(w.persist.passenger_slots.drain());
        self.entity_attachments.extend(w.entity_attachments.drain());
        self.died.extend(w.died.drain());
        self.love_cause.extend(w.love_cause.drain());
        self.last_hurt_by_player
            .extend(w.last_hurt_by_player.drain());
        self.lead_killers.extend(w.lead_killers.drain());
        // What the region's tick left for the server's game, after the server's own.
        self.level_events.append(&mut w.level_events);
        self.pending.absorb(std::mem::take(&mut w.pending));
        self.spill.absorb(std::mem::take(&mut w.spill));
        self.creatures
            .lan_sounds
            .append(&mut w.creatures.lan_sounds);
        self.creatures
            .lan_particles
            .append(&mut w.creatures.lan_particles);
        self.lan_events.append(&mut w.lan_events);
        self.lan_block_events.append(&mut w.lan_block_events);
        self.lan_entity_events.append(&mut w.lan_entity_events);
        self.lan_critical_hits.append(&mut w.lan_critical_hits);
        self.lan_cushion_breaks.append(&mut w.lan_cushion_breaks);
        self.lan_songs.append(&mut w.lan_songs);
        self.lan_kinetic_hits.append(&mut w.lan_kinetic_hits);
        self.lan_global_events.append(&mut w.lan_global_events);
        self.level_teleports.append(&mut w.level_teleports);
        self.server_trips.append(&mut w.server_trips);
        std::mem::take(&mut w.rand_sequences)
    }
}

/// Whether the level's world phases run by regions on threads of their own. Off unless asked for by name:
/// `HORIZON_REGIONS=1` (`[regions] enabled = true` in `server.toml` sets it; `0` or unset is off), so the generic
/// parallel tick setting never splits a world. It also needs the parallel tick on (`HORIZON_PARALLEL_TICK=0`
/// keeps every tick serial) and `HORIZON_REGION_THREADS=0` turns it off.
fn region_threads_on() -> bool {
    #[cfg(test)]
    if tests::REGIONS_FORCED.with(std::cell::Cell::get) {
        return true;
    }
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        std::env::var("HORIZON_REGIONS").is_ok_and(|v| matches!(v.trim(), "1" | "on" | "true"))
            && std::env::var("HORIZON_REGION_THREADS").map_or(true, |v| v.trim() != "0")
    }) && horizon_client::parallel::parallel_tick() > 0
}

impl Game<'_> {
    /// The world phases of this tick by regions, on threads of their own. With players in two or more separate
    /// areas of the level, it is split into regions: each region's chunks, entities and players move into a
    /// region game that keeps them (a resident region) and ticks them on a thread of its own, its players' packets
    /// and network turn included. They stay there, splitting and merging by themselves, and come back only when
    /// something needs the server's thread (a chat line or command, a join or leave, a death).
    /// The serial phases that follow always run, on what this thread still holds: what no region owns, and the
    /// whole level in a tick the regions are back.
    pub(super) fn tick_world_regions(&mut self) -> Result<()> {
        // The End is one region while its dragon fight runs: the fight belongs to the server's thread. So is a
        // level with a raid near a player, or whose raids are not read yet (`raids_let_regions`).
        let fighting = self.dim == Dim::End && !(self.fight.killed || self.fight.previously_killed);
        let raiding = !self.raids_let_regions();
        if self.regions.worker {
            return Ok(());
        }
        // The global tick: what the regions sent since, applied to the level before it ticks.
        if region_threads_on() {
            self.record_home_census();
            self.tick_global();
        }
        // The server's own ids come from the shared pool whenever its block runs low, whichever level ticks and
        // split or not: a level that is never split (the End) spawning past its block would hand out the ids
        // the region games of other levels are about to take.
        if self.regions.entity_id_end != 0
            && let Some(pool) = self.regions.entity_pool.clone()
        {
            self.refill_entity_ids(&pool);
        }
        if !region_threads_on() || fighting || raiding {
            // The parallel tick went off, the End's fight or a raid began, with regions out: they come back, or
            // their chunks, entities and players stay frozen away from the server.
            self.land_level_residents();
            return Ok(());
        }
        let dim = self.dim;
        let entered = std::time::Instant::now();
        if self.regions.flight.as_ref().is_some_and(|f| f.0 == dim) {
            // Started early and still out: the tick's phases on what no region owns run meanwhile, and
            // `finish_flight_here` waits for the regions before the server's hub ticks.
            return Ok(());
        }
        if let Some(set) = self.regions.resident.remove(&dim) {
            self.regions.prof.note_resident();
            if let Some(flight) = self.resident_launch(set, false) {
                // Out on their threads: the phases that follow overlap them (`finish_flight_here`).
                self.regions.flight = Some((dim, flight));
            }
            // Landed whole (something needs the server's thread): the level ticks on this thread, and is split
            // again at the start of the next tick.
            return Ok(());
        }
        // A player a command or a native mod just sent to another level waits for the server's hand to move them
        // (`run_level_teleports`, later in this tick): lent to a region now, they would not be found and the
        // teleport would be lost. The level is split again once it has run.
        if !self.level_teleports.is_empty() {
            return Ok(());
        }
        if self.regions.paused > 0 {
            self.regions.paused -= 1;
            return Ok(());
        }
        let Some(host) = self.lan.as_ref() else {
            return Ok(());
        };
        let view = host.hub.config.view_distance.clamp(2, 32);
        let players: Vec<(i32, i32)> = host
            .hub
            .guests
            .iter()
            .filter(|g| host.hub.here(g))
            .map(|g| {
                (
                    (g.position[0].floor() as i32) >> 4,
                    (g.position[2].floor() as i32) >> 4,
                )
            })
            .collect();
        if players.len() < 2 {
            return Ok(());
        }
        let levels = self.tickets.levels();
        let ticking = self
            .world
            .chunks
            .keys()
            .copied()
            .filter(|&(x, z)| levels.block_ticking(x, z));
        // One chunk wider than a player's view, as `residents_must_land` asks for: a player standing still
        // never counts as having left its region.
        let regions = Regions::new(ticking, &players, view + 1);
        if regions.count < 2 {
            return Ok(());
        }
        static LAST: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        if LAST.swap(regions.count, Ordering::Relaxed) != regions.count {
            log::info!(
                "ticking {} regions for {} players",
                regions.count,
                players.len()
            );
        }
        // Games for the regions: made on demand, kept for the next ticks.
        self.collect_region_games();
        let have = self.regions.spares.len();
        if have < regions.count {
            // Not enough games yet: they are made meanwhile, and this tick runs as one.
            let coming = self.regions.factory.len();
            let wanted = (regions.count - have).saturating_sub(coming).min(4);
            if wanted > 0 {
                self.request_region_games(wanted);
            }
            return Ok(());
        }
        let games: Vec<Box<Game<'static>>> = (0..regions.count)
            .filter_map(|_| self.regions.spares.take())
            .collect();
        let pool = Arc::clone(
            self.regions
                .entity_pool
                .get_or_insert_with(|| Arc::new(AtomicU32::new(self.next_entity + 1))),
        );
        if self.regions.entity_id_end == 0 {
            self.regions.entity_id_end = self.next_entity;
        }
        self.refill_entity_ids(&pool);
        // Each region's players, by connection, split off once.
        let region_of_conn: HashMap<u32, usize> = self
            .lan
            .as_ref()
            .map(|h| {
                h.hub
                    .guests
                    .iter()
                    .filter(|g| h.hub.here(g))
                    .filter_map(|g| {
                        let chunk = (
                            (g.position[0].floor() as i32) >> 4,
                            (g.position[2].floor() as i32) >> 4,
                        );
                        Some((g.conn, regions.of_chunk(chunk)?))
                    })
                    .collect()
            })
            .unwrap_or_default();
        let mut host = self.lan.take().expect("checked above");
        let mut split_guests = host
            .hub
            .split_guests(regions.count, |conn| region_of_conn.get(&conn).copied());
        let mut trackers = host
            .hub
            .lend_trackers(regions.count, |chunk| regions.of_chunk(chunk))
            .into_iter();
        let split = self.split_level(&regions, false);
        let shared = Arc::new(SetShared::default());
        let mut set = ResidentSet {
            games: Vec::with_capacity(regions.count),
            lent: Default::default(),
            shared: Arc::clone(&shared),
        };
        let mut lent: FxHashSet<(i32, i32)> = FxHashSet::default();
        for (k, ((part, mut w), (chunks, guests))) in split
            .parts
            .into_iter()
            .zip(games)
            .zip(
                split
                    .chunks
                    .into_iter()
                    .zip(split_guests.iter_mut().map(std::mem::take)),
            )
            .enumerate()
        {
            let mob_ids: Vec<u32> = part.mobs.iter().map(|m| m.id).collect();
            self.lend_region(&mut w, part);
            // The level's own settings: a game made for another dimension earlier is this one's now.
            w.copy_world_settings_from(self);
            w.refill_entity_ids(&pool);
            self.lend_maps(&mut w);
            let conns: Vec<u32> = guests.iter().map(|g| g.conn).collect();
            host.hub.away.extend(conns.iter().copied());
            let mut shell = w.lan.take().unwrap_or_else(|| host.region_shell());
            host.lend_region(&mut shell, &conns, &mob_ids, dim, guests);
            if let Some(tracker) = trackers.next() {
                shell.hub.set_tracker(tracker);
            }
            w.lan = Some(shell);
            w.bind_lan_residents();
            // The region claims its sections in the level's map: from here on it owns them.
            let id = self.regions.ids.fetch_add(1, Ordering::Relaxed) + 1;
            let area = regions.area_of(k);
            for (&section, &kind) in &area {
                shared.map.claim(id, section, kind);
            }
            w.regions.area = area;
            w.regions.topology_seen = None;
            w.regions.link = Some(RegionLink {
                shared: Arc::clone(&shared),
                id,
                spares: Arc::clone(&self.regions.spares),
                ids: Arc::clone(&self.regions.ids),
                pool: Arc::clone(&pool),
            });
            if let Some(link) = w.regions.link.clone() {
                w.own_region_streaming(&link);
            }
            lent.extend(chunks.iter().map(|c| (c.x, c.z)));
            let mut slot = Slot::new(w, id);
            slot.conns = conns;
            slot.fresh = true;
            set.games.push(slot);
        }
        set.lent = Arc::new(lent);
        self.lan = Some(host);
        self.regions.prof.note_resident();
        self.regions.prof.add(Step::Split, entered);
        if let Some(flight) = self.resident_launch(set, true) {
            self.regions.flight = Some((dim, flight));
        }
        Ok(())
    }

    /// Starts the tick of the resident regions `set` on their threads, without waiting for them: `None` once
    /// they are back (the tick ran by landing them).
    fn resident_launch(&mut self, mut set: ResidentSet, first: bool) -> Option<Flight> {
        if !first {
            // The regions that came home alone last tick are lent again, or let go when no player is left in them.
            let relending = std::time::Instant::now();
            let mut relent = false;
            let mut k = 0;
            while k < set.games.len() {
                if set.games[k].landed {
                    // A player whose command waits for another region's answer stays home until it has run.
                    if self.lan.as_ref().is_some_and(|h| {
                        h.deferred_commands
                            .iter()
                            .any(|(c, ..)| set.games[k].conns.contains(&c.conn))
                    }) {
                        k += 1;
                        continue;
                    }
                    relent = true;
                    if !self.relend_slot(&mut set, k) {
                        continue;
                    }
                }
                k += 1;
            }
            if relent {
                self.regions.prof.add(Step::Split, relending);
            }
            if set.games.is_empty() {
                // The last region had no player left.
                self.land_resident_set(set);
                return None;
            }
            self.route_region_chat(Some(&mut set));
            // Players handed off to this level's regions, and those that arrived in it (a command, a portal):
            // each goes to the region that owns where they stand.
            self.deliver_set_handoffs(&mut set);
            self.route_strays(&mut set);
            match self.residents_must_land(&set) {
                Some(Land::All(why)) => {
                    self.regions.prof.landed_because(why);
                    let landing = std::time::Instant::now();
                    horizon_client::spans::time("regions_land", || self.land_resident_set(set));
                    self.regions.prof.add(Step::Land, landing);
                    return None;
                }
                Some(Land::Slots(list)) => {
                    let landing = std::time::Instant::now();
                    for (k, why) in list {
                        self.regions.prof.landed_because(why);
                        self.land_slot(&mut set, k);
                    }
                    self.regions.prof.add(Step::Land, landing);
                    // The players that came home alone are not away for the request the launch clears.
                    if self.lan.as_ref().is_some_and(|h| h.hub.wake_request) {
                        self.wake_guests();
                    }
                }
                None => {}
            }
            // Regions within the merge radius of each other pause at this tick boundary, and one absorbs the other.
            let merging = std::time::Instant::now();
            if self.merge_regions(&mut set) {
                self.regions.prof.add(Step::Split, merging);
            }
        }
        self.stock_spares();
        self.streaming.lent = Some(Arc::clone(&set.lent));
        // The server's own streamer keeps to what no resident region owns; a region's streamer loads, lights and
        // saves its own, the chunks that are home are the server's again.
        set.shared.set_landed(
            set.games
                .iter()
                .filter(|s| s.landed)
                .map(|s| s.uid),
        );
        let own = Arc::clone(&set.shared);
        self.streaming.own = Some(Arc::new(move |x, z| own.server_owns((x, z))));
        // The level as its regions read it at the start of their ticks.
        self.publish_global();
        self.deliver_away_out_home();
        let pool = Arc::clone(
            self.regions
                .entity_pool
                .get_or_insert_with(|| Arc::new(AtomicU32::new(self.next_entity + 1))),
        );
        // The server's own game runs on past its block while the set stays out: it takes a new one from the pool
        // whenever it runs low, as the region games do, so its ids never reach a region game's block.
        if self.regions.entity_id_end == 0 {
            self.regions.entity_id_end = self.next_entity;
        }
        self.refill_entity_ids(&pool);
        let started = std::time::Instant::now();
        // A region still ticking from an earlier tick is left to finish; the others start.
        let mut failed: Option<String> = None;
        let mut launched = Vec::new();
        for (k, slot) in set.games.iter_mut().enumerate() {
            match slot.home(Some(started)) {
                Err(e) => {
                    failed.get_or_insert(e);
                    continue;
                }
                Ok(false) => {
                    // The region watchdog: a region tick past 5 s is told of every 5 s.
                    if slot.since.elapsed() >= std::time::Duration::from_secs(5)
                        && slot
                            .told
                            .is_none_or(|t| t.elapsed() >= std::time::Duration::from_secs(5))
                    {
                        slot.told = Some(started);
                        let at = slot.places.first().copied().unwrap_or_default();
                        log::warn!(
                            "region {k} near chunk {},{} has ticked for {:.1} s ({} players)",
                            at.0,
                            at.1,
                            slot.since.elapsed().as_secs_f32(),
                            slot.places.len()
                        );
                    }
                    continue;
                }
                Ok(true) => {}
            }
            if slot.landed {
                continue;
            }
            let Some(mut w) = slot.game.take() else {
                continue;
            };
            let copying = std::time::Instant::now();
            // The clock, the weather, the rules and the border are the global tick's: the region reads its
            // snapshot at the start of its own tick (`Game::read_global`). A game just lent is given the
            // tick's other settings (its ticket levels) here, while it is still home.
            if first || std::mem::take(&mut slot.fresh) {
                w.copy_tick_settings_from(self);
            }
            w.refill_entity_ids(&pool);
            // The chunks kept loaded whatever the distance (`/forceload`, a mod's ticket) in the sections this
            // region owns are its streamer's to keep.
            if !self.streaming.forced.is_empty() || !w.streaming.forced.is_empty() {
                let mine: std::collections::HashSet<(i32, i32)> = self
                    .streaming
                    .forced
                    .iter()
                    .copied()
                    .filter(|&c| {
                        set.shared
                            .map
                            .owner_of_chunk(c)
                            .is_some_and(|o| o.id == slot.uid)
                    })
                    .collect();
                if w.streaming.forced != mine {
                    w.streaming.forced = mine;
                }
            }
            if let (Some(shell), Some(host)) = (w.lan.as_mut(), self.lan.as_mut()) {
                shell.hub.copy_region_settings(&host.hub);
                host.hub.give_away(&mut shell.hub);
                for line in slot.mail.drain(..) {
                    shell.hub.deliver_chat(&line);
                }
                shell.hub.wake_request |= host.hub.wake_request;
                self.regions.away_out.retain(|(uuid, packet, horizon)| {
                    let Some(g) = shell
                        .hub
                        .guests
                        .iter()
                        .find(|g| g.placed && g.uuid == *uuid)
                    else {
                        return true;
                    };
                    if !*horizon || g.brand == "horizon" {
                        shell.hub.send_to(g.conn, packet);
                    }
                    false
                });
            }
            self.regions.prof.add(Step::Copy, copying);
            let spawning = std::time::Instant::now();
            let (tx, rx) = std::sync::mpsc::sync_channel(1);
            slot.running = Some(rx);
            slot.since = started;
            slot.told = None;
            // A thread that cannot start leaves the game here: it holds the region's chunks and entities.
            let cell = Arc::new(std::sync::Mutex::new(Some(w)));
            let job = Arc::clone(&cell);
            let spawned = super::region_pool::run(move || {
                let Some(mut w) = job
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .take()
                else {
                    return;
                };
                let began = std::time::Instant::now();
                let mut report = Report::default();
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    w.tick_region_phases().map_err(|e| e.to_string())?;
                    // The end of its own tick: the sections it needs, a split, a merge to ask for.
                    report = w.region_topology();
                    if std::mem::take(&mut w.regions.conns_dirty) && report.conns.is_none() {
                        report.conns = w
                            .lan
                            .as_ref()
                            .map(|h| h.hub.guests.iter().map(|g| g.conn).collect());
                    }
                    Ok(())
                }))
                .unwrap_or_else(|_| Err("a region thread panicked".to_owned()));
                let _ = tx.send(Done {
                    game: w,
                    result,
                    took: began.elapsed().as_secs_f32() * 1000.0,
                    report,
                });
            });
            self.regions.prof.add(Step::Spawn, spawning);
            if !spawned {
                slot.game = cell
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .take();
                slot.running = None;
                failed.get_or_insert("a region thread could not start".to_owned());
            } else {
                launched.push(k);
            }
        }
        // The regions a late region split off before it came home.
        set.take_in_born();
        // A night skip's wake-up request went into every region that started; one set after this point (by the
        // sleep tick that follows an early launch) stays for the next launch. A region still running keeps it
        // for its next start too.
        if failed.is_none()
            && set
                .games
                .iter()
                .enumerate()
                .all(|(k, s)| s.landed || launched.contains(&k))
            && let Some(host) = self.lan.as_mut()
        {
            host.hub.wake_request = false;
        }
        Some(Flight {
            set,
            launched,
            failed,
            started,
        })
    }

    /// Starts the resident regions of the level being ticked before the rest of its server-side work (streaming,
    /// the tick's prelude, the network of the connections no region owns), so that work overlaps their tick;
    /// [`Game::finish_flight`] waits for them. Only for a set that stays out this tick: one that must land, or
    /// has a region back alone, is left to [`Game::tick_world_regions`].
    pub(super) fn launch_residents_early(&mut self) {
        let dim = self.dim;
        if !region_threads_on() || self.regions.worker || self.regions.flight.is_some() {
            return;
        }
        // The prelude's tick and scheduled functions run commands over the whole level: they take the old path,
        // with the regions started after them.
        if self.level_pass.first && self.functions_run_this_tick() {
            return;
        }
        let Some(set) = self.regions.resident.get(&dim) else {
            return;
        };
        if set.games.iter().any(|s| s.landed) || self.residents_must_land(set).is_some() {
            return;
        }
        let Some(set) = self.regions.resident.remove(&dim) else {
            return;
        };
        self.regions.prof.note_resident();
        if let Some(flight) = self.resident_launch(set, false) {
            self.regions.flight = Some((dim, flight));
        }
    }

    /// [`Game::finish_flight`] for the level in place: the one whose regions the tick's phases overlapped, so the
    /// wait is only for the part of their tick those phases did not cover. A flight of another level (a phase
    /// that entered it and has not come back) is left for the level's own tick.
    pub(super) fn finish_flight_here(&mut self) -> Result<()> {
        if self
            .regions
            .flight
            .as_ref()
            .is_some_and(|f| f.0 == self.dim)
        {
            self.finish_flight()?;
        }
        Ok(())
    }

    /// Waits for the regions [`Game::launch_residents_early`] started (until the regions' deadline) and keeps
    /// the set resident: the second half of their tick.
    pub(super) fn finish_flight(&mut self) -> Result<()> {
        let Some((dim, flight)) = self.regions.flight.take() else {
            return Ok(());
        };
        if let Some(set) = self.resident_finish(flight)? {
            self.regions.resident.insert(dim, set);
        }
        Ok(())
    }

    /// Waits for the regions of `flight` until the deadline: what runs past it carries on by itself.
    fn resident_finish(&mut self, flight: Flight) -> Result<Option<ResidentSet>> {
        let Flight {
            mut set,
            launched,
            mut failed,
            started,
        } = flight;
        let waiting = std::time::Instant::now();
        // Waited for until the deadline: what runs past it carries on by itself.
        let deadline = started + region_deadline();
        horizon_client::spans::time("regions_wait", || {
            for k in launched {
                if let Err(e) = set.games[k].home(Some(deadline)) {
                    failed.get_or_insert(e);
                }
            }
        });
        self.regions.prof.add(Step::Wait, waiting);
        set.take_in_born();
        self.route_region_chat(Some(&mut set));
        super::health::record_regions(
            self.dim.id(),
            set.games
                .iter()
                .map(|s| super::health::RegionStat {
                    level: self.dim.id().to_owned(),
                    at: s.places.first().map_or((0, 0), |c| (c.0 << 4, c.1 << 4)),
                    chunks: s.chunks,
                    players: s.places.len(),
                    ms: if s.running.is_some() {
                        s.since.elapsed().as_secs_f32() * 1000.0
                    } else {
                        s.took
                    },
                    late: s.running.is_some(),
                })
                .collect(),
        );
        // The sleep counts the regions reported, for the server's night skip; the wake-up went out with them.
        for slot in set.games.iter_mut().filter(|s| !s.landed) {
            if let Some(w) = slot.game.as_ref()
                && let Some(shell) = w.lan.as_ref()
            {
                slot.sleep = shell.hub.away_sleep;
            }
        }
        let (mut a, mut s, mut d) = (0, 0, 0);
        for slot in set.games.iter().filter(|s| !s.landed) {
            a += slot.sleep.0;
            s += slot.sleep.1;
            d += slot.sleep.2;
        }
        if let Some(host) = self.lan.as_mut() {
            host.hub.away_sleep = (a, s, d);
        }
        // Native mods read the players away in a region from what it last reported.
        if self.lan.is_some() {
            for slot in set.games.iter_mut().filter(|s| !s.landed) {
                if let Some(w) = slot.game.as_ref() {
                    slot.entities = w.entity_count();
                }
                if let Some(shell) = slot.game.as_ref().and_then(|w| w.lan.as_ref()) {
                    slot.players = shell
                        .hub
                        .guests
                        .iter()
                        .filter(|g| g.placed && g.participates_online())
                        .map(|g| AwayPlayer {
                            conn: g.conn,
                            id: g.entity_id as u32,
                            uuid: g.uuid,
                            sender: g.sender.clone(),
                        })
                        .collect();
                    slot.status = shell
                        .hub
                        .guests
                        .iter()
                        .filter(|g| g.participates_online())
                        .map(|g| AwayStatus {
                            name: g.name.clone(),
                            uuid: g.uuid,
                            address: g.address,
                            latency: Arc::clone(&g.latency),
                            dimension: g.dimension.clone(),
                            position: g.position,
                        })
                        .collect();
                }
            }
        }
        if let Some(e) = failed {
            // A region that panicked or could not start does not fail the server's tick: the regions come home,
            // the level ticks on this thread, and splitting waits a while before it is tried again.
            log::error!(
                "a region of {} failed ({e}): the level ticks whole on the server thread for {FAILED_PAUSE} ticks",
                self.dim.id()
            );
            let landing = std::time::Instant::now();
            self.land_resident_set(set);
            self.regions.prof.add(Step::Land, landing);
            self.regions.paused = FAILED_PAUSE;
            return Ok(None);
        }
        Ok(Some(set))
    }

    /// Whether resident regions have to come back before the next tick: all of them, or only some. No timer is
    /// among the reasons: a region comes home because the server's thread has something to do with it.
    fn residents_must_land(&self, set: &ResidentSet) -> Option<Land> {
        let Some(host) = self.lan.as_ref() else {
            return Some(Land::All("no host"));
        };
        let slot_of = |conn: u32| set.games.iter().position(|s| s.conns.contains(&conn));
        let mut slots: Vec<(usize, &'static str)> = Vec::new();
        let mut add = |k: usize, why: &'static str| {
            if !slots.iter().any(|s| s.0 == k) {
                slots.push((k, why));
            }
        };
        if host.hub.away_chat_pending() {
            return Some(Land::All("chat"));
        }
        match host.hub.away_server_conns() {
            None => return Some(Land::All("packet")),
            Some(conns) => {
                // A native mod hook for a player (a command, a mod message, a leave) may reach any other
                // player or their entities (`/invsee`, a trade, a voice group): every region comes home.
                if !conns.is_empty() && host.hub.mods_loaded {
                    return Some(Land::All("native mod"));
                }
                for c in conns {
                    match slot_of(c) {
                        Some(k) => add(k, "packet"),
                        None => return Some(Land::All("packet")),
                    }
                }
            }
        }
        // A player who is in the server's hands and belongs to no region.
        if host
            .hub
            .guests
            .iter()
            .any(|g| host.hub.here(g) && slot_of(g.conn).is_none())
        {
            return Some(Land::All("guest here"));
        }
        if !self.level_teleports.is_empty() {
            return Some(Land::All("teleport"));
        }
        for (k, s) in set.games.iter().enumerate() {
            if s.landed {
                continue;
            }
            let Some(w) = s.game.as_ref() else {
                continue;
            };
            if wants_land(w) {
                add(k, "region wants");
            } else if let Some(why) = s.wants {
                // Its own thread found no player left in it.
                add(k, why);
            }
        }
        (!slots.is_empty()).then_some(Land::Slots(slots))
    }

    /// Region `k` alone back in the server's level: it ticks there with the server's thread this tick, and is
    /// lent again at the start of the next ([`Game::relend_slot`]). The other regions stay out.
    fn land_slot(&mut self, set: &mut ResidentSet, k: usize) {
        let slot = &mut set.games[k];
        let _ = slot.home(None);
        set.take_in_born();
        let slot = &mut set.games[k];
        let Some(mut w) = slot.game.take() else {
            return;
        };
        let mut host = self.lan.take();
        if let Some(shell) = w.lan.as_mut() {
            for line in slot.mail.drain(..) {
                shell.hub.deliver_chat(&line);
            }
        }
        let chunks = chunks_of(&w);
        let drawn = self.take_region_back(&mut w, &chunks);
        self.rand_sequences.absorb(drawn);
        if let (Some(host), Some(shell)) = (host.as_mut(), w.lan.as_mut()) {
            host.hub.park_tracker(&mut shell.hub);
            host.take_region_back(shell);
        }
        if let Some(host) = host.as_mut() {
            host.hub.release_away_conns(&slot.conns);
            host.hub.restore_guest_order();
            // Its sleepers are counted among the level's players now, not among those the region reported.
            take_sleep(&mut host.hub.away_sleep, std::mem::take(&mut slot.sleep));
        }
        slot.game = Some(w);
        slot.landed = true;
        slot.wants = None;
        self.lan = host;
        self.bind_lan_residents();
        // The set of lent chunks loses this region's, in place (the streamer's copy is let go first).
        self.streaming.lent = None;
        let lent = Arc::make_mut(&mut set.lent);
        for c in &chunks {
            lent.remove(&(c.x, c.z));
        }
        self.streaming.lent = Some(Arc::clone(&set.lent));
    }

    /// Region `k`, landed alone, split out of the level again in the area it owns: its chunks (the ones loaded
    /// since too), its entities and its players, moved back into its game. False when no player of the server's
    /// is left in its area: the region is let go (its sections are free, its game spare).
    fn relend_slot(&mut self, set: &mut ResidentSet, k: usize) -> bool {
        let dim = self.dim;
        let Some(mut w) = set.games[k].game.take() else {
            return true;
        };
        let Some(mut host) = self.lan.take() else {
            set.games[k].game = Some(w);
            return true;
        };
        let pool = Arc::clone(
            self.regions
                .entity_pool
                .get_or_insert_with(|| Arc::new(AtomicU32::new(self.next_entity + 1))),
        );
        // The split reads this region's sections alone, as part 0 of 1: it costs what the region holds, not
        // what the other regions or the level's count of regions add.
        let id = set.games[k].uid;
        let layout = Regions::for_area(&w.regions.area, id as usize);
        let in_region: HashMap<u32, usize> = host
            .hub
            .guests
            .iter()
            .filter(|g| host.hub.here(g))
            .filter_map(|g| Some((g.conn, layout.of_chunk(g.chunk())?)))
            .collect();
        if in_region.is_empty() {
            // Nobody is in it any more.
            self.lan = Some(host);
            set.shared.map.release_all(id);
            self.regions.spares.give(w);
            set.games.remove(k);
            // What was queued for the region goes to the server's game: its players are gone, and so is it.
            self.regions
                .tasks
                .merge_region(id, super::region_tasks::SERVER);
            return false;
        }
        let guests = host
            .hub
            .split_guests(1, |conn| in_region.get(&conn).copied())
            .into_iter()
            .next()
            .unwrap_or_default();
        let tracker = host
            .hub
            .lend_trackers(1, |chunk| layout.of_chunk(chunk))
            .into_iter()
            .next();
        let split = self.split_level(&layout, false);
        let part = split
            .parts
            .into_iter()
            .next()
            .expect("a part for the region");
        let chunks = split.chunks.into_iter().next().unwrap_or_default();
        let mob_ids: Vec<u32> = part.mobs.iter().map(|m| m.id).collect();
        self.lend_region(&mut w, part);
        w.copy_world_settings_from(self);
        w.refill_entity_ids(&pool);
        self.lend_maps(&mut w);
        let conns: Vec<u32> = guests.iter().map(|g| g.conn).collect();
        host.hub.away.extend(conns.iter().copied());
        let mut shell = w.lan.take().unwrap_or_else(|| host.region_shell());
        host.lend_region(&mut shell, &conns, &mob_ids, dim, guests);
        if let Some(tracker) = tracker {
            shell.hub.set_tracker(tracker);
        }
        w.lan = Some(shell);
        w.bind_lan_residents();
        self.lan = Some(host);
        self.streaming.lent = None;
        let lent = Arc::make_mut(&mut set.lent);
        lent.extend(chunks.iter().map(|c| (c.x, c.z)));
        let slot = &mut set.games[k];
        slot.game = Some(w);
        slot.landed = false;
        slot.fresh = true;
        slot.conns = conns;
        self.streaming.lent = Some(Arc::clone(&set.lent));
        true
    }

    /// The resident regions of the level in place back: their parts of the level and their players.
    fn land_resident_set(&mut self, mut set: ResidentSet) {
        self.route_region_chat(Some(&mut set));
        self.streaming.lent = None;
        // Everything is home: the server's streamer owns the level again, once the regions' chunks are in.
        self.streaming.own = None;
        // One still ticking is waited for, and the regions it split off are among the set.
        for slot in &mut set.games {
            let _ = slot.home(None);
        }
        set.take_in_born();
        let mut host = self.lan.take();
        let mut back = Vec::with_capacity(set.games.len());
        // Whose packets were held for this set's regions: only theirs are released below. Another level's resident
        // regions keep theirs away.
        let mut conns: Vec<u32> = Vec::new();
        for mut slot in set.games {
            conns.extend(slot.conns.iter().copied());
            // The region is home for good: what was queued for it is the server's game's now.
            self.regions
                .tasks
                .merge_region(slot.uid, super::region_tasks::SERVER);
            let Some(mut w) = slot.game.take() else {
                continue;
            };
            if slot.landed {
                // Home already, alone.
                back.push(w);
                continue;
            }
            if let Some(shell) = w.lan.as_mut() {
                for line in slot.mail.drain(..) {
                    shell.hub.deliver_chat(&line);
                }
            }
            let chunks = chunks_of(&w);
            let drawn = self.take_region_back(&mut w, &chunks);
            self.rand_sequences.absorb(drawn);
            if let (Some(host), Some(shell)) = (host.as_mut(), w.lan.as_mut()) {
                host.hub.park_tracker(&mut shell.hub);
                host.take_region_back(shell);
            }
            if let Some(host) = host.as_mut() {
                take_sleep(&mut host.hub.away_sleep, std::mem::take(&mut slot.sleep));
            }
            back.push(w);
        }
        if let Some(host) = host.as_mut() {
            // Not `release_away`: it frees every away connection, those of another level's resident regions too,
            // whose players' packets (their commands among them) then found no guest at the next poll and were
            // dropped.
            host.hub.release_away_conns(&conns);
            host.hub.restore_guest_order();
        }
        self.lan = host;
        self.bind_lan_residents();
        for w in back {
            self.regions.spares.give(w);
        }
        self.deliver_away_out_home();
        self.wake_returned_guests();
        // The regions of this level are home: /tps has no last ticks of them to show.
        super::health::record_regions(self.dim.id(), Vec::new());
    }

    /// A night skip's wake-up request that no region took (it came after their start) wakes the players now home.
    fn wake_returned_guests(&mut self) {
        if self
            .lan
            .as_mut()
            .is_some_and(|h| std::mem::take(&mut h.hub.wake_request))
        {
            self.wake_guests();
        }
    }

    /// The players native mods can see: those away in a resident region, as it last reported them.
    pub(super) fn away_server_players(&self) -> impl Iterator<Item = &AwayPlayer> {
        self.regions
            .resident
            .values()
            .chain(self.regions.flight.iter().map(|(_, f)| &f.set))
            .flat_map(|set| set.games.iter().filter(|s| !s.landed))
            .flat_map(|s| s.players.iter())
    }

    /// The players away in a resident region, for `server-status`, as the region last reported them.
    pub(super) fn away_status_players(&self) -> impl Iterator<Item = &AwayStatus> {
        self.regions
            .resident
            .values()
            .chain(self.regions.flight.iter().map(|(_, f)| &f.set))
            .flat_map(|set| set.games.iter().filter(|s| !s.landed))
            .flat_map(|s| s.status.iter())
    }

    /// The chunks and entities the resident regions of level `dim` hold, as they last reported them: what the
    /// level in place lacks for `server-status` while they are away.
    pub(super) fn resident_totals(&self, dim: Dim) -> (usize, usize) {
        self.regions
            .resident
            .get(&dim)
            .into_iter()
            .chain(
                self.regions
                    .flight
                    .iter()
                    .filter(move |(d, _)| *d == dim)
                    .map(|(_, f)| &f.set),
            )
            .flat_map(|set| set.games.iter().filter(|s| !s.landed))
            .fold((0, 0), |(c, e), s| (c + s.chunks, e + s.entities))
    }

    /// Queues `packet` for a player away in a region, for its next tick; false when `uuid` is not away.
    /// `horizon_only`: only a Horizon client gets it.
    pub(super) fn queue_for_away(
        &mut self,
        uuid: u128,
        packet: horizon_protocol::packets::play::Clientbound,
        horizon_only: bool,
    ) -> bool {
        let away = self.away_server_players().any(|player| player.uuid == uuid);
        if away {
            // Never dropped: they wait for the region's next tick or for its landing.
            self.regions.away_out.push((uuid, packet, horizon_only));
        }
        away
    }

    /// Carries the chat lines players said to the players who were not there: a line a region's hub relayed to its
    /// own players goes to the server's own players and into the mail of every other resident region (sent when
    /// the region is next lent); a line the server's hub relayed while regions were out goes into every region's
    /// mail. `mine` is the resident set being ticked, which is not among `self.regions.resident`.
    fn route_region_chat(&mut self, mine: Option<&mut ResidentSet>) {
        // (the region it was said in, 0 for the server's hub; the line)
        let mut lines: Vec<(u64, horizon_client::lan::hub::chat::RegionChat)> = Vec::new();
        if let Some(host) = self.lan.as_mut() {
            lines.extend(host.hub.region_chat_out.drain(..).map(|l| (0, l)));
        }
        let mut sets: Vec<&mut ResidentSet> = Vec::new();
        sets.extend(mine);
        sets.extend(self.regions.resident.values_mut());
        sets.extend(self.regions.flight.as_mut().map(|(_, f)| &mut f.set));
        for set in &mut sets {
            for slot in set.games.iter_mut().filter(|s| !s.landed) {
                if let Some(shell) = slot.game.as_mut().and_then(|w| w.lan.as_mut()) {
                    lines.extend(shell.hub.region_chat_out.drain(..).map(|l| (slot.uid, l)));
                }
            }
        }
        for (origin, line) in lines {
            if origin != 0
                && let Some(host) = self.lan.as_mut()
            {
                host.hub.deliver_chat(&line);
            }
            // A region that came home alone has its players in the server's hub, which has them already.
            for slot in sets
                .iter_mut()
                .flat_map(|set| set.games.iter_mut())
                .filter(|s| !s.landed && s.uid != origin)
            {
                slot.mail.push(line.clone());
            }
        }
    }

    /// What was queued for players who are home now goes out.
    fn deliver_away_out_home(&mut self) {
        let Some(host) = self.lan.as_ref() else {
            return;
        };
        self.regions.away_out.retain(|(uuid, packet, horizon)| {
            let Some(g) = host.hub.guests.iter().find(|g| g.placed && g.uuid == *uuid) else {
                return true;
            };
            if !*horizon || g.brand == "horizon" {
                host.hub.send_to(g.conn, packet);
            }
            false
        });
    }

    /// How many entities the level in place holds now, with those lent to its resident regions that are home to be
    /// counted (a region still ticking past its deadline is not). Only this level's: another level's regions hold
    /// entities this level's save never writes. `None` while none of its regions is out.
    pub(super) fn entities_with_residents(&self) -> Option<usize> {
        let dim = self.dim;
        let sets = || {
            self.regions
                .resident
                .get(&dim)
                .into_iter()
                .chain(
                    self.regions
                        .flight
                        .iter()
                        .filter(move |(d, _)| *d == dim)
                        .map(|(_, f)| &f.set),
                )
                .filter(|set| !set.games.is_empty())
        };
        // Nothing is lent: nothing a save could miss.
        sets().next()?;
        let lent: usize = sets()
            .flat_map(|set| set.games.iter())
            .filter(|s| !s.landed)
            .filter_map(|s| s.game.as_ref())
            .map(|w| w.live_entity_ids().len())
            .sum();
        Some(self.live_entity_ids().len() + lent)
    }

    /// Brings every resident region home when this tick runs datapack functions (the `load` and `tick` tags, or a
    /// scheduled one due): their commands (`execute as @a`) read the whole server's players and chunks, which
    /// regions away would hide. The regions are lent again after the prelude.
    pub(super) fn land_residents_for_functions(&mut self) {
        if self.regions.worker
            || (self.regions.resident.is_empty() && self.regions.flight.is_none())
            || !self.functions_run_this_tick()
        {
            return;
        }
        self.land_residents();
    }

    /// Brings every resident region home before a native mod runs a command (`/tp`, `/execute`...): its selectors
    /// and teleports find players and chunks in the server's own game only, and a player away in a region is
    /// not there, so the command would fail ("no entity was found"). A no-op where nothing is away.
    pub(super) fn land_residents_for_native_command(&mut self) {
        if self.regions.worker
            || (self.regions.resident.is_empty() && self.regions.flight.is_none())
        {
            return;
        }
        self.land_residents();
    }

    /// Whether `dim` has resident regions (lent players, entities and chunks), those just started early included.
    pub(super) fn level_has_residents(&self, dim: Dim) -> bool {
        self.regions
            .resident
            .get(&dim)
            .is_some_and(|set| !set.games.is_empty())
            || self.regions.flight.as_ref().is_some_and(|f| f.0 == dim)
    }

    /// The resident regions of the level being ticked back, those just started early included.
    fn land_level_residents(&mut self) {
        let dim = self.dim;
        if self.regions.flight.as_ref().is_some_and(|f| f.0 == dim)
            && let Some((dim, flight)) = self.regions.flight.take()
        {
            self.regions.resident.insert(dim, flight.set);
        }
        if let Some(set) = self.regions.resident.remove(&dim) {
            self.land_resident_set(set);
        }
    }

    /// Every resident region of every level back (before a save or a stop).
    pub(super) fn land_residents(&mut self) {
        let home = self.dim;
        // Regions started early this tick (`launch_residents_early`) are in `flight`, not resident: an autosave
        // in the tick's prelude, a command's save or a stop found nothing to land and wrote the level without
        // them. Landing waits for their threads.
        if let Some((dim, flight)) = self.regions.flight.take() {
            self.regions.resident.insert(dim, flight.set);
        }
        let dims: Vec<Dim> = self.regions.resident.keys().copied().collect();
        for dim in dims {
            if dim != self.dim && self.enter_level(dim).is_err() {
                continue;
            }
            if let Some(set) = self.regions.resident.remove(&dim) {
                self.land_resident_set(set);
            }
        }
        if self.dim != home {
            let _ = self.enter_level(home);
        }
    }

    /// One region game's phases of the tick: the level's entities and blocks in its chunks, around its players.
    pub(super) fn tick_region_phases(&mut self) -> Result<()> {
        // The level's clock, weather, rules and border as the global tick last published them.
        self.read_global();
        // What other regions asked of this one, before the tick's own work.
        self.run_region_tasks();
        self.level_pass = super::levels::Pass {
            first: false,
            last: false,
            running: Some(true),
            packets_drained: false,
        };
        // Its own chunks: the ones its players' areas need are loaded, generated, lit and unloaded here, on this
        // thread and the shared workers, as the server's tick does for the chunks no region owns.
        self.update_dedicated_streaming();
        self.update_world();
        self.update_chunk_tickets();
        // The anticheat's timer is given its players' time since their last tick here: a region's hub does not
        // poll the network, where the server's hub does it.
        if let Some(host) = self.lan.as_mut() {
            host.hub.anticheat_tick(self.clock.rate());
        }
        // Its players' packets first, before its block ticks, as vanilla.
        self.tick_lan_packets();
        self.begin_ordered_ticks();
        self.tick_blocks()?;
        self.tick_paintings();
        self.tick_item_frames();
        self.tick_armor_stands()?;
        self.tick_cushions();
        self.tick_boats();
        self.tick_carts();
        self.tick_thunder();
        self.tick_lecterns();
        self.tick_bolts();
        self.tick_fishing();
        self.tick_tridents();
        self.tick_rockets();
        self.tick_ominous_item_spawners();
        let original_item_moves = self.tick_items();
        self.hoppers_catch_items(&original_item_moves);
        self.touch_arrows();
        self.tick_mobs()?;
        self.recheck_golem_openers();
        self.run_ordered_ticks(horizon_client::join_seq::JoinSeq::MAX, false)?;
        self.tick_experience();
        self.tick_moving_pistons()?;
        self.tick_block_entities()?;
        // Its players' turn: their own ticks, entities, chunks and block changes, from its own world.
        self.tick_lan(false);
        // Primed TNT and falling blocks that joined since the spawn packets (the early pass leaves them
        // out until `announced` passes them), then everything joined counts as announced, as the server's
        // tick does (#10063).
        self.run_ordered_ticks(horizon_client::join_seq::JoinSeq::MAX, true)?;
        self.apply_late_wind_pushes();
        self.ordered.announced = horizon_client::join_seq::JoinSeq::upcoming();
        // Sculk in its own chunks: the vibrations its players and mobs make, and the sensors, shriekers and
        // catalysts that answer them.
        self.tick_sculk_entities();
        self.tick_sculk_blocks()?;
        // Sleep: the night is skipped by the server, which sums what the regions report; a skipped night wakes
        // this region's players too.
        if self
            .lan
            .as_mut()
            .is_some_and(|h| std::mem::take(&mut h.hub.wake_request))
        {
            self.wake_guests();
        }
        if let Some(h) = self.lan.as_mut() {
            h.hub.away_sleep = (0, 0, 0);
        }
        let counts = self.sleep_counts();
        if let Some(h) = self.lan.as_mut() {
            h.hub.away_sleep = counts;
        }
        // The light its block changes made is worked out by its own streamer at the start of the next tick
        // (`update_world`), as the server's is for the chunks it holds: small edits at once, a big batch (a
        // `/fill`) by the workers.
        // What the global tick needs of this region: its living players and its mob counts.
        self.send_census();
        Ok(())
    }
}

impl Game<'_> {
    /// The end of a region game's own tick: the sections its players and ticking chunks need now, worked out on
    /// its own thread against the level's [`SectionMap`] that every region reads.
    ///
    /// - A region whose sections fall into separate groups splits itself: each group but the one with most
    ///   players is handed to a spare game ([`Game::carve_into`]). A game for it may be missing; the region
    ///   then tries again at the end of its next tick.
    /// - Sections it needs and no region owns are claimed; a section another region owns, or one within the merge
    ///   radius of another region's, asks for a merge with that region, done at the next tick boundary
    ///   ([`Game::merge_regions`]).
    /// - Sections it no longer needs are given up once enough pile up ([`SHRINK_SLACK`]).
    ///
    /// The chunks of an area that grew are loaded by the region's own streamer; no landing is needed for them.
    fn region_topology(&mut self) -> Report {
        let mut report = Report::default();
        let Some(link) = self.regions.link.clone() else {
            return report;
        };
        let Some(host) = self.lan.as_ref() else {
            return report;
        };
        let view = host.hub.config.view_distance.clamp(2, 32) + 1;
        let players: Vec<(i32, i32)> = host
            .hub
            .guests
            .iter()
            .filter(|g| host.hub.here(g))
            .map(|g| g.chunk())
            .collect();
        if players.is_empty() {
            // Nobody left in it (a leave, a death, a player sent away): the server's thread lets it go.
            report.wants = Some("empty");
            return report;
        }
        // Nothing to work out while no player has moved to another chunk and the chunks that tick are the
        // same: the common tick costs a pass over the chunks.
        let levels = self.tickets.levels();
        let mix = |a: i32, b: i32| {
            (a as u32 as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)
                ^ (b as u32 as u64).wrapping_mul(0xC2B2_AE3D_27D4_EB4F)
        };
        let mut seen = u64::from(view as u32);
        for &(x, z) in &players {
            seen = seen.wrapping_add(mix(x, z).rotate_left(11));
        }
        let ticking = self
            .world
            .chunks
            .keys()
            .copied()
            .filter(|&(x, z)| levels.block_ticking(x, z));
        let mut count = 0u64;
        for (x, z) in ticking.clone() {
            seen = seen.wrapping_add(mix(x >> SECTION_SHIFT, z >> SECTION_SHIFT));
            count += 1;
        }
        seen = seen.wrapping_add(count.rotate_left(32));
        if self.regions.topology_seen == Some(seen) {
            return report;
        }
        let cut = Regions::new(ticking, &players, view);
        let mut players_in = vec![0usize; cut.count];
        for &p in &players {
            if let Some(r) = cut.of_chunk(p) {
                players_in[r] += 1;
            }
        }
        let keep = (0..cut.count)
            .max_by_key(|&g| (players_in[g], std::cmp::Reverse(g)))
            .unwrap_or(0);
        let mut carved_all = true;
        for g in (0..cut.count).filter(|&g| g != keep) {
            let Some(dest) = link.spares.take() else {
                carved_all = false;
                break;
            };
            match self.carve_into(dest, cut.area_of(g), &link) {
                Ok(born) => {
                    log::info!("regions: region {} split off region {}", link.id, born.id);
                    report.born.push(born)
                }
                Err(dest) => {
                    link.spares.give(dest);
                    carved_all = false;
                    break;
                }
            }
        }
        if !report.born.is_empty() {
            report.conns = self
                .lan
                .as_ref()
                .map(|h| h.hub.guests.iter().map(|g| g.conn).collect());
        }
        if !carved_all {
            // Tried again at the end of the next tick, with the games the server's thread makes meanwhile.
            return report;
        }
        self.regions.topology_seen = Some(seen);
        let desired = cut.area_of(keep);
        // What it needs and does not own yet: claimed now, or a merge with whoever owns it.
        let mut owners: FxHashSet<RegionId> = FxHashSet::default();
        for (&section, &kind) in &desired {
            if self
                .regions
                .area
                .get(&section)
                .is_some_and(|&have| have >= kind)
            {
                continue;
            }
            match link.shared.map.claim(link.id, section, kind) {
                Claim::Held => {
                    let have = self.regions.area.entry(section).or_insert(kind);
                    *have = (*have).max(kind);
                }
                Claim::Taken(o) => {
                    owners.insert(o.id);
                }
            }
        }
        // What it holds and no longer needs, once it is a lot.
        let stale: Vec<_> = self
            .regions
            .area
            .keys()
            .filter(|s| !desired.contains_key(s))
            .copied()
            .collect();
        if stale.len() >= SHRINK_SLACK {
            for section in stale {
                link.shared.map.release(link.id, section);
                self.regions.area.remove(&section);
            }
        }
        // A player standing in a chunk another region owns: they ran into its area before this check could run.
        for &p in players.iter().filter(|&&p| cut.of_chunk(p) == Some(keep)) {
            if let Some(o) = link.shared.map.owner_of_chunk(p)
                && o.id != link.id
            {
                owners.insert(o.id);
            }
        }
        // Claimed first and read after, so of two regions that grow towards each other at once, at least one
        // sees the other.
        owners.extend(link.shared.map.neighbours(link.id, &self.regions.area));
        for other in owners {
            link.shared.ask_merge(link.id, other);
        }
        report
    }

    /// Splits region `area` off this region game into `dest`, a spare game: the area's chunks, entities, block
    /// ticks and players, with the sections handed over in the map. The existing lend primitives do it, with this
    /// game as the one that lends. The game back, when nobody can be lent to it.
    fn carve_into(
        &mut self,
        mut dest: Box<Game<'static>>,
        area: Area,
        link: &RegionLink,
    ) -> std::result::Result<Born, Box<Game<'static>>> {
        let Some(mut host) = self.lan.take() else {
            return Err(dest);
        };
        let id = link.ids.fetch_add(1, Ordering::Relaxed) + 1;
        let dim = self.dim;
        let layout = Regions::for_area(&area, (id as usize) << 20);
        let conns: Vec<u32> = host
            .hub
            .guests
            .iter()
            .filter(|g| host.hub.here(g) && layout.of_chunk(g.chunk()).is_some())
            .map(|g| g.conn)
            .collect();
        let guests = host.hub.hand_guests(&conns);
        let tracker = host
            .hub
            .lend_trackers(1, |chunk| layout.of_chunk(chunk))
            .into_iter()
            .next();
        // Every chunk of the area goes: a region game's chunks are all finished.
        let split = self.split_level(&layout, false);
        let part = split
            .parts
            .into_iter()
            .next()
            .expect("a part for the region");
        let mob_ids: Vec<u32> = part.mobs.iter().map(|m| m.id).collect();
        self.lend_region(&mut dest, part);
        dest.copy_world_settings_from(self);
        dest.refill_entity_ids(&link.pool);
        self.lend_maps(&mut dest);
        let mut shell = dest.lan.take().unwrap_or_else(|| host.region_shell());
        host.lend_region(&mut shell, &conns, &mob_ids, dim, guests);
        if let Some(tracker) = tracker {
            shell.hub.set_tracker(tracker);
        }
        dest.lan = Some(shell);
        self.lan = Some(host);
        dest.bind_lan_residents();
        self.bind_lan_residents();
        for (&section, &kind) in &area {
            if !link.shared.map.hand(link.id, id, section, kind) {
                link.shared.map.claim(id, section, kind);
            }
            self.regions.area.remove(&section);
        }
        dest.regions.area = area;
        dest.regions.topology_seen = None;
        let born_link = RegionLink { id, ..link.clone() };
        dest.own_region_streaming(&born_link);
        dest.regions.link = Some(born_link);
        Ok(Born {
            game: dest,
            id,
            conns,
        })
    }

    /// Region game `b`, merged into this one: everything it holds (its part of the level, its entities, its
    /// players and what is kept for them) and its sections. The existing take-back primitive does it, with this
    /// game as the one that takes: nothing passes through the server's game.
    fn absorb_region(&mut self, b: &mut Game<'_>) {
        let chunks = chunks_of(b);
        let drawn = self.take_region_back(b, &chunks);
        self.rand_sequences.absorb(drawn);
        if let (Some(host), Some(shell)) = (self.lan.as_mut(), b.lan.as_mut()) {
            host.hub.park_tracker(&mut shell.hub);
            host.take_peer_region(shell);
        }
        for (section, kind) in b.regions.area.drain() {
            let have = self.regions.area.entry(section).or_insert(kind);
            *have = (*have).max(kind);
        }
        self.regions.topology_seen = None;
        self.bind_lan_residents();
    }

    /// The regions that asked for a merge ([`Game::region_topology`]), merged where both are at their tick
    /// boundary: home, not landed, not still ticking. One that is not waits for the next tick. The one with more
    /// chunks absorbs the other. Whether any merged.
    fn merge_regions(&mut self, set: &mut ResidentSet) -> bool {
        let asked: Vec<(RegionId, RegionId)> = set
            .shared
            .merges
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .copied()
            .collect();
        let mut merged = false;
        for (a, b) in asked {
            let position =
                |set: &ResidentSet, id: RegionId| set.games.iter().position(|s| s.uid == id);
            let forget = |set: &ResidentSet| {
                set.shared
                    .merges
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(&(a, b));
            };
            let (Some(ia), Some(ib)) = (position(set, a), position(set, b)) else {
                // One of them is gone (merged into another, or let go): the next area check asks again if need be.
                forget(set);
                continue;
            };
            let now = std::time::Instant::now();
            for i in [ia, ib] {
                let _ = set.games[i].home(Some(now));
            }
            set.take_in_born();
            let ready = |s: &Slot| s.running.is_none() && !s.landed && s.game.is_some();
            if !(ready(&set.games[ia]) && ready(&set.games[ib])) {
                continue;
            }
            forget(set);
            let (keep, gone) = if set.games[ia].chunks >= set.games[ib].chunks {
                (ia, ib)
            } else {
                (ib, ia)
            };
            let mut gone_slot = set.games.remove(gone);
            let keep = if gone < keep { keep - 1 } else { keep };
            let slot = &mut set.games[keep];
            let (Some(mut big), Some(mut small)) = (slot.game.take(), gone_slot.game.take()) else {
                continue;
            };
            big.absorb_region(&mut small);
            log::info!(
                "regions: region {} merged into region {}",
                gone_slot.uid,
                slot.uid
            );
            set.shared.map.reassign(gone_slot.uid, slot.uid);
            self.regions.tasks.merge_region(gone_slot.uid, slot.uid);
            slot.conns.append(&mut gone_slot.conns);
            slot.mail.append(&mut gone_slot.mail);
            slot.players.append(&mut gone_slot.players);
            slot.places.append(&mut gone_slot.places);
            slot.sleep = (
                slot.sleep.0 + gone_slot.sleep.0,
                slot.sleep.1 + gone_slot.sleep.1,
                slot.sleep.2 + gone_slot.sleep.2,
            );
            slot.chunks += gone_slot.chunks;
            slot.wants = slot.wants.or(gone_slot.wants);
            slot.game = Some(big);
            self.regions.spares.give(small);
            merged = true;
        }
        merged
    }
}

/// The chunks region game `w` holds.
fn chunks_of(w: &Game<'_>) -> Vec<ChunkPos> {
    w.world
        .chunks
        .keys()
        .map(|&(x, z)| ChunkPos::new(x, z))
        .collect()
}

/// Takes a landed region's reported sleep counts out of the total the server's hub keeps for the regions away.
fn take_sleep(total: &mut (i64, i64, i64), region: (i64, i64, i64)) {
    total.0 = (total.0 - region.0).max(0);
    total.1 = (total.1 - region.1).max(0);
    total.2 = (total.2 - region.2).max(0);
}

fn split<T>(v: &mut Vec<T>, regions: &Lending<'_>, at: impl Fn(&T) -> (f64, f64)) -> Vec<Vec<T>> {
    let places: Vec<Option<usize>> = v
        .iter()
        .map(|item| {
            let (x, z) = at(item);
            regions.of_block(x, z)
        })
        .collect();
    let mut sizes = vec![0usize; regions.parts()];
    for r in places.iter().flatten() {
        sizes[*r] += 1;
    }
    let mut out: Vec<Vec<T>> = sizes.iter().map(|&n| Vec::with_capacity(n)).collect();
    let mut kept = Vec::new();
    for (item, place) in v.drain(..).zip(places) {
        match place {
            Some(r) => out[r].push(item),
            None => kept.push(item),
        }
    }
    *v = kept;
    out
}

/// What a split lends of `layout`: each region's part of the level, but for the chunks the server's streaming has
/// not finished (`held`: not lit, or not post-processed yet) and what stands or is kept in them, which stay with the
/// level. A region game neither lights nor post-processes chunks, and its hub sends its players only finished ones:
/// a chunk lent unfinished was never sent while its region stayed resident, and a player teleported to new ground
/// waited for it for ever. A held chunk goes to its region with the region's next relend once it is finished.
pub(super) struct Lending<'a> {
    layout: &'a Regions,
    held: &'a FxHashSet<(i32, i32)>,
}

impl Lending<'_> {
    fn parts(&self) -> usize {
        self.layout.parts()
    }

    fn only_region(&self) -> Option<usize> {
        self.layout.only_region()
    }

    /// The region chunk `key` and what is in it are lent to, if any.
    fn of_chunk(&self, key: (i32, i32)) -> Option<usize> {
        if self.held.contains(&key) {
            return None;
        }
        self.layout.of_chunk(key)
    }

    fn of_block(&self, x: f64, z: f64) -> Option<usize> {
        self.of_chunk(((x.floor() as i32) >> 4, (z.floor() as i32) >> 4))
    }
}

/// What goes to each region from the items `take` removes: `at` an item's place, `lend` takes the items
/// whose place a region owns out of the list (in its order) and says which region each went to.
fn by_region<T>(
    regions: &Lending<'_>,
    at: impl Fn(&T) -> (f64, f64),
    lend: impl FnOnce(&mut dyn FnMut(&T) -> bool) -> Vec<T>,
) -> Vec<Vec<T>> {
    let mut places: Vec<usize> = Vec::new();
    let taken = lend(&mut |item| {
        let (x, z) = at(item);
        match regions.of_block(x, z) {
            Some(r) => {
                places.push(r);
                true
            }
            None => false,
        }
    });
    let mut out: Vec<Vec<T>> = (0..regions.parts()).map(|_| Vec::new()).collect();
    for (item, r) in taken.into_iter().zip(places) {
        out[r].push(item);
    }
    out
}

/// Split a joined list without replacing the list's residency binding. Only the items that move are touched:
/// the ones that stay keep their place and their membership.
fn split_list<T: Joined, const DIRECT: bool>(
    list: &mut JoinList<T, DIRECT>,
    regions: &Lending<'_>,
    at: impl Fn(&T) -> (f64, f64),
) -> Vec<Vec<T>> {
    by_region(regions, at, |take| list.extract_if(take).collect())
}

fn split_resident<T: Joined>(
    list: &mut ResidentList<T>,
    regions: &Lending<'_>,
    at: impl Fn(&T) -> (f64, f64),
) -> Vec<Vec<T>> {
    by_region(regions, at, |take| list.extract_if(take).collect())
}

/// A map's entries whose block a region owns, moved into that region's map.
fn split_map<K: std::hash::Hash + Eq + Copy, V>(
    m: &mut HashMap<K, V>,
    regions: &Lending<'_>,
    at: impl Fn(&K) -> (i32, i32),
) -> Vec<HashMap<K, V>> {
    let mut out: Vec<HashMap<K, V>> = (0..regions.parts()).map(|_| HashMap::new()).collect();
    for (k, v) in m.extract_if(|k, _| regions.of_chunk(at(k)).is_some()) {
        let r = regions.of_chunk(at(&k)).expect("filtered");
        out[r].insert(k, v);
    }
    out
}

fn fx_split<K: std::hash::Hash + Eq + Copy, V>(
    m: &mut FxHashMap<K, V>,
    regions: &Lending<'_>,
    at: impl Fn(&K) -> (i32, i32),
) -> Vec<FxHashMap<K, V>> {
    let mut out: Vec<FxHashMap<K, V>> = (0..regions.parts()).map(|_| Default::default()).collect();
    for (k, v) in m.extract_if(|k, _| regions.of_chunk(at(k)).is_some()) {
        let r = regions.of_chunk(at(&k)).expect("filtered");
        out[r].insert(k, v);
    }
    out
}

fn block_chunk(p: (i32, i32, i32)) -> (i32, i32) {
    (p.0 >> 4, p.2 >> 4)
}
fn array_chunk(p: &[i32; 3]) -> (i32, i32) {
    (p[0] >> 4, p[2] >> 4)
}

/// One region's part of a level, split out of the server's game ([`Game::split_level`]).
pub(super) struct RegionPart {
    /// The chunks of the part already scanned for creaking hearts.
    pub(super) scanned: horizon_client::fast_hash::FxHashSet<(i32, i32)>,
    pub(super) world: horizon_client::world::World,
    pub(super) ticks: horizon_world::level::Ticks,
    pub(super) sections: horizon_client::random_tick_sections::RandomTickSections,
    pub(super) sequences: horizon_client::random_sequences::RandomSequences,
    pub(super) mobs: Vec<Mob>,
    pub(super) dropped: Vec<super::ItemEntity>,
    pub(super) orbs: Vec<super::ExperienceOrb>,
    pub(super) projectiles: Vec<horizon_client::projectiles::Projectile>,
    pub(super) falling: Vec<super::FallingBlock>,
    pub(super) paintings: Vec<horizon_client::entities::Painting>,
    pub(super) item_frames: Vec<horizon_client::entities::ItemFrame>,
    pub(super) armor_stands: Vec<horizon_client::entities::ArmorStand>,
    pub(super) cushions: Vec<horizon_client::entities::Cushion>,
    pub(super) others: Vec<super::commands::OtherEntity>,
    pub(super) boats: Vec<super::boats::Boat>,
    pub(super) carts: Vec<super::minecarts::Minecart>,
    pub(super) bolts: Vec<super::lightning::Bolt>,
    pub(super) crystals: Vec<super::dragon::Crystal>,
    pub(super) breath_clouds: Vec<super::dragon::Breath>,
    pub(super) tridents: Vec<super::trident::ThrownTrident>,
    pub(super) rockets: Vec<super::fireworks::Rocket>,
    pub(super) ominous_spawners: Vec<super::ominous_item_spawner::OminousItemSpawner>,
    pub(super) tnts: Vec<super::tnt::PrimedTnt>,
    pub(super) hoppers: HashMap<(i32, i32, i32), super::machines::HopperState>,
    pub(super) jukebox_sounds: HashMap<(i32, i32, i32), u64>,
    pub(super) lid_openers: HashMap<(i32, i32, i32), i32>,
    pub(super) knot_ids: HashMap<[i32; 3], u32>,
    pub(super) lectern_pulses: Vec<((i32, i32, i32), i32)>,
    pub(super) anims: horizon_client::block_entity_anim::BlockEntityAnims,
}

/// A level split into its regions' parts, and the chunks each region holds (for the block ticks' return).
pub(super) struct LevelSplit {
    pub parts: Vec<RegionPart>,
    pub chunks: Vec<Vec<ChunkPos>>,
}

impl Game<'_> {
    /// The level in place split into `layout`'s parts: each region's chunks, block entities, ticks and
    /// entities moved out of this game's level; what no region owns stays, and so do the chunks the streaming has
    /// not finished, with what is in them ([`Lending`]).
    ///
    /// `hold_unfinished`: the streaming of this game's level has chunks it has not finished (the server's game).
    /// A region game's chunks are all finished: a part split out of one moves every chunk it owns.
    pub(super) fn split_level(&mut self, layout: &Regions, hold_unfinished: bool) -> LevelSplit {
        // A split restricted to one region reads the chunks of its sections alone, not the level's every chunk: the
        // cost of a relend follows the region, not the level.
        let scope: Option<Vec<(i32, i32)>> = layout.only_region().map(|k| layout.chunk_keys(k));
        let unfinished = |c: &horizon_client::chunk::Chunk| !c.lit || !c.postprocess.is_empty();
        let held: FxHashSet<(i32, i32)> = match &scope {
            _ if !hold_unfinished => FxHashSet::default(),
            Some(keys) => keys
                .iter()
                .filter(|key| self.world.chunks.get(key).is_some_and(|c| unfinished(c)))
                .copied()
                .collect(),
            None => self
                .world
                .chunks
                .iter()
                .filter(|(_, c)| unfinished(c))
                .map(|(&key, _)| key)
                .collect(),
        };
        let regions = &Lending {
            layout,
            held: &held,
        };
        let n = regions.parts();
        let first = layout.band + regions.only_region().unwrap_or(0);
        let of_chunk = |key: (i32, i32)| regions.of_chunk(key);
        // Entities read from chunk files still wait in the world: they join the level first (the step a tick
        // takes), or they would be added to chunks this game no longer holds once those went to a region.
        self.spawn_loaded_entities();
        let mut chunks: Vec<Vec<ChunkPos>> = vec![Vec::new(); n];
        match &scope {
            Some(keys) => {
                for &(x, z) in keys {
                    if self.world.chunks.contains_key(&(x, z))
                        && let Some(r) = of_chunk((x, z))
                    {
                        chunks[r].push(ChunkPos::new(x, z));
                    }
                }
            }
            None => {
                for &(x, z) in self.world.chunks.keys() {
                    if let Some(r) = of_chunk((x, z)) {
                        chunks[r].push(ChunkPos::new(x, z));
                    }
                }
            }
        }
        let seeds: Vec<i64> = (0..n)
            .map(|_| horizon_core::random::RandomSource::next_long(&mut self.world.random))
            .collect();
        let tick_seeds: Vec<i32> = seeds.iter().map(|s| *s as i32).collect();
        // The world's chunks, the scheduled ticks and the random-tick sections move on threads of their own at
        // once (each touches only its own part of the level), the entities are sorted meanwhile on this one.
        let (world, block_ticks, sections_of) = (
            &mut self.world,
            &mut self.block_ticks,
            &mut self.random_tick_sections,
        );
        let (chunks_ref, seeds_ref, tick_seeds_ref) = (&chunks, &seeds, &tick_seeds);
        let scope_ref = scope.as_deref();
        let (worlds, ticks, sections) = std::thread::scope(|scope| {
            let worlds = scope.spawn(move || world.lend(n, of_chunk, seeds_ref, scope_ref));
            let ticks = scope.spawn(move || block_ticks.lend(chunks_ref, tick_seeds_ref, first));
            let sections = sections_of.lend(n, of_chunk, scope_ref);
            match (worlds.join(), ticks.join()) {
                (Ok(w), Ok(t)) => (w, t, sections),
                (Err(e), _) | (_, Err(e)) => std::panic::resume_unwind(e),
            }
        });
        let mut sequences = self.rand_sequences.split(n + 1);
        // The level's own sequences stay with the level (the first of the split).
        self.rand_sequences = sequences.remove(0);
        let v3 = |p: &horizon_core::geometry::Vec3| (p.x, p.z);
        let a3 = |p: &[f64; 3]| (p[0], p[2]);
        let b3 = |p: &[i32; 3]| (f64::from(p[0]), f64::from(p[2]));
        let mut mobs = split_list(&mut self.mobs, regions, |m| v3(&m.position));
        let mut dropped = split_list(&mut self.dropped, regions, |d| v3(&d.position));
        let mut orbs = split_list(&mut self.orbs, regions, |o| v3(&o.position));
        let mut projectiles =
            split_list(
                &mut self.creatures.projectiles,
                regions,
                |p| v3(&p.position),
            );
        let mut falling = split_list(&mut self.falling, regions, |f| v3(&f.position));
        let mut paintings = split_list(&mut self.paintings, regions, |p| b3(&p.pos));
        let mut item_frames = split_list(&mut self.item_frames, regions, |f| b3(&f.pos));
        let mut armor_stands = split_list(&mut self.armor_stands, regions, |s| v3(&s.position));
        let mut cushions = split_list(&mut self.cushions, regions, |c| v3(&c.position));
        let mut others = split_resident(&mut self.others, regions, |o| v3(&o.position));
        let mut boats = split_list(&mut self.boats, regions, |b| v3(&b.position));
        let mut carts = split_list(&mut self.carts, regions, |c| v3(&c.position));
        let mut bolts = split(&mut self.bolts, regions, |b| a3(&b.at));
        let mut crystals = split_list(&mut self.crystals, regions, |c| v3(&c.position));
        let mut breath_clouds = split_resident(&mut self.breath_clouds, regions, |b| a3(&b.at));
        let mut tridents = split_list(&mut self.tridents, regions, |t| v3(&t.position));
        let mut rockets = split_list(&mut self.rockets, regions, |r| v3(&r.position));
        let mut ominous_spawners =
            split_resident(&mut self.ominous_spawners, regions, |o| a3(&o.position));
        let mut tnts = split_list(&mut self.tnts, regions, |t| v3(&t.position));
        let mut hoppers = split_map(&mut self.hoppers, regions, |p| block_chunk(*p));
        let mut jukebox_sounds = split_map(&mut self.jukebox_sounds, regions, |p| block_chunk(*p));
        let mut lid_openers = split_map(&mut self.lid_openers, regions, |p| block_chunk(*p));
        let mut knot_ids = split_map(&mut self.knot_ids, regions, array_chunk);
        let mut scanned = self
            .creaking
            .split_scanned(n, |key| regions.of_chunk(key), scope_ref);
        let mut lectern_pulses: Vec<Vec<((i32, i32, i32), i32)>> =
            (0..n).map(|_| Vec::new()).collect();
        self.lectern_pulses
            .retain(|&(p, t)| match regions.of_chunk(block_chunk(p)) {
                Some(r) => {
                    lectern_pulses[r].push((p, t));
                    false
                }
                None => true,
            });
        let a = &mut self.be_anims;
        let mut chests = fx_split(&mut a.chests, regions, array_chunk);
        let mut books = fx_split(&mut a.books, regions, array_chunk);
        let mut shulkers = fx_split(&mut a.shulkers, regions, array_chunk);
        let mut bells = fx_split(&mut a.bells, regions, array_chunk);
        let mut conduits = fx_split(&mut a.conduits, regions, array_chunk);
        let mut skulls = fx_split(&mut a.skulls, regions, array_chunk);
        let mut pots = fx_split(&mut a.pots, regions, array_chunk);
        let mut gateway_servers = fx_split(&mut a.gateway_servers, regions, array_chunk);
        let mut conduit_servers = fx_split(&mut a.conduit_servers, regions, array_chunk);
        let mut parts = Vec::with_capacity(n);
        for (r, ((world, ticks), sections)) in
            worlds.into_iter().zip(ticks).zip(sections).enumerate()
        {
            parts.push(RegionPart {
                world,
                ticks,
                sections,
                sequences: sequences.remove(0),
                mobs: std::mem::take(&mut mobs[r]),
                dropped: std::mem::take(&mut dropped[r]),
                orbs: std::mem::take(&mut orbs[r]),
                projectiles: std::mem::take(&mut projectiles[r]),
                falling: std::mem::take(&mut falling[r]),
                paintings: std::mem::take(&mut paintings[r]),
                item_frames: std::mem::take(&mut item_frames[r]),
                armor_stands: std::mem::take(&mut armor_stands[r]),
                cushions: std::mem::take(&mut cushions[r]),
                others: std::mem::take(&mut others[r]),
                boats: std::mem::take(&mut boats[r]),
                carts: std::mem::take(&mut carts[r]),
                bolts: std::mem::take(&mut bolts[r]),
                crystals: std::mem::take(&mut crystals[r]),
                breath_clouds: std::mem::take(&mut breath_clouds[r]),
                tridents: std::mem::take(&mut tridents[r]),
                rockets: std::mem::take(&mut rockets[r]),
                ominous_spawners: std::mem::take(&mut ominous_spawners[r]),
                tnts: std::mem::take(&mut tnts[r]),
                hoppers: std::mem::take(&mut hoppers[r]),
                jukebox_sounds: std::mem::take(&mut jukebox_sounds[r]),
                lid_openers: std::mem::take(&mut lid_openers[r]),
                knot_ids: std::mem::take(&mut knot_ids[r]),
                lectern_pulses: std::mem::take(&mut lectern_pulses[r]),
                scanned: std::mem::take(&mut scanned[r]),
                anims: horizon_client::block_entity_anim::BlockEntityAnims {
                    chests: std::mem::take(&mut chests[r]),
                    books: std::mem::take(&mut books[r]),
                    shulkers: std::mem::take(&mut shulkers[r]),
                    bells: std::mem::take(&mut bells[r]),
                    conduits: std::mem::take(&mut conduits[r]),
                    skulls: std::mem::take(&mut skulls[r]),
                    pots: std::mem::take(&mut pots[r]),
                    gateway_servers: std::mem::take(&mut gateway_servers[r]),
                    conduit_servers: std::mem::take(&mut conduit_servers[r]),
                    ..Default::default()
                },
            });
        }
        LevelSplit { parts, chunks }
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    thread_local! {
        /// A test that ticks a whole server by regions turns them on for its own thread: the environment
        /// variable is read once for the process.
        pub(in crate::game) static REGIONS_FORCED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }

    #[test]
    fn thousands_of_lends_across_seventy_regions_take_ids_only_as_they_are_used() {
        let pool = AtomicU32::new(1);
        // Every region game's (next id, end of its block).
        let mut games = vec![(0u32, 0u32); 70];
        let mut blocks: Vec<(u32, u32)> = Vec::new();
        let mut used = 0u64;
        for cycle in 0..5000usize {
            for (k, (next, end)) in games.iter_mut().enumerate() {
                // A lend: the game tops up its block when it runs low.
                let before = *end;
                refill_id_block(&pool, next, end);
                if *end != before {
                    blocks.push((*next, *end));
                }
                // Its tick spawns a few entities; the landing gives nothing back.
                for _ in 0..1 + (cycle * 7 + k * 13) % 40 {
                    *next += 1;
                    assert!(*next < *end, "a tick ran past its block");
                    used += 1;
                }
            }
        }
        let handed = u64::from(pool.load(Ordering::Relaxed));
        assert!(
            handed < u64::from(u32::MAX) / 100,
            "{handed} ids handed out for {used} used"
        );
        // Whatever a game leaves of its blocks is lost, never reused: no two blocks share an id.
        blocks.sort_unstable();
        assert!(blocks.windows(2).all(|w| w[0].1 <= w[1].0));
        assert!(blocks.len() > 70, "the games took more than one block each");
    }

    #[test]
    #[ignore = "daily"]
    fn a_player_who_stays_dead_lands_their_region_once_for_the_death_and_not_again_until_they_respawn()
     {
        crate::game::native_guest_turn_tests::with_guests(|game| {
            let wants = |game: &Game<'_>| wants_land(game);
            assert!(!wants(game), "two living players want nothing");
            let host = game.lan.as_mut().unwrap();
            host.hub.update_guest(12, |g| g.health = 0.0).unwrap();
            assert!(
                wants(game),
                "a player dying: the death is for the server's thread"
            );
            // The region goes out with the player and comes home for it.
            let host = game.lan.as_mut().unwrap();
            let mut region = host.hub.region_shell();
            let out = host
                .hub
                .split_guests(1, |conn| (conn == 12).then_some(0))
                .remove(0);
            region.guests.extend(out);
            host.hub.take_guests_back(&mut region);
            host.hub.update_guest(12, |g| g.dead = true).unwrap();
            for _ in 0..100 {
                assert!(
                    !wants(game),
                    "a dead player's region is lent again and stays out"
                );
            }
            // Their respawn is theirs to ask for; once alive, the next death lands once more.
            game.guest_respawn(12);
            let g = game.lan.as_ref().unwrap().hub.guest(12).unwrap();
            assert!(!g.dead && !g.death_landed);
            game.lan
                .as_mut()
                .unwrap()
                .hub
                .update_guest(12, |g| g.health = 0.0)
                .unwrap();
            assert!(wants(game), "a second death lands again");
        });
    }

    #[test]
    #[ignore = "daily"]
    fn tps_and_mspt_are_answered_by_the_region_of_the_player_who_typed_them() {
        use horizon_protocol::packets::play::{ChatCommand, Serverbound};
        crate::game::native_guest_turn_tests::with_guests(|game| {
            let hub = &game.lan.as_ref().unwrap().hub;
            let typed = |line: &str| {
                Serverbound::ChatCommand(ChatCommand {
                    command: line.to_owned(),
                })
            };
            assert!(hub.away_local(&typed("tps")));
            assert!(hub.away_local(&typed("mspt")));
            // What reaches other players or chunks still brings every region home.
            assert!(!hub.away_local(&typed("tps extra")));
            assert!(!hub.away_local(&typed("kill @a")));
            assert!(!hub.away_local(&typed("tp Bob")));
        });
    }

    #[test]
    fn a_landed_regions_sleepers_leave_the_away_count() {
        let mut total = (5, 3, 2);
        take_sleep(&mut total, (2, 1, 1));
        assert_eq!(total, (3, 2, 1));
        take_sleep(&mut total, (9, 9, 9));
        assert_eq!(total, (0, 0, 0));
    }

    #[test]
    fn players_far_apart_are_separate_regions() {
        let players = [(0, 0), (400, 0)];
        let r = Regions::new(players, &players, 0);
        assert_eq!(r.count, 2);
        assert_ne!(r.of_chunk((0, 0)), r.of_chunk((400, 0)));
        assert_eq!(r.of_chunk((200, 0)), None);
    }

    #[test]
    fn a_regions_chunk_keys_are_the_chunks_of_the_sections_it_owns() {
        let players = [(0, 0), (400, 0)];
        let r = Regions::new(players, &players, 0);
        for k in 0..r.count {
            let keys = r.chunk_keys(k);
            // A ticking section and the ring around it: 9 sections of 4 x 4 chunks.
            assert_eq!(keys.len(), 9 * 16);
            assert!(keys.iter().all(|&key| r.of_chunk(key) == Some(k)));
        }
    }

    #[test]
    fn ticking_sections_two_apart_merge_three_apart_do_not() {
        // Sections are 4 chunks wide: chunk 0 is section 0, chunk 8 is section 2, chunk 12 is section 3.
        let two = Regions::new([(0, 0), (8, 0)], &[(0, 0), (8, 0)], 0);
        assert_eq!(two.count, 1);
        let three = Regions::new([(0, 0), (12, 0)], &[(0, 0), (12, 0)], 0);
        assert_eq!(three.count, 2);
    }

    #[test]
    fn a_ticking_section_owns_the_ring_around_it() {
        let r = Regions::new([(0, 0)], &[(0, 0)], 0);
        // Section 0 and its eight neighbours: chunks -4..=7 on each side.
        assert_eq!(r.of_chunk((-4, 7)), Some(0));
        assert_eq!(r.of_chunk((7, -4)), Some(0));
        assert_eq!(r.of_chunk((8, 0)), None);
        assert_eq!(r.of_chunk((-5, 0)), None);
    }

    #[test]
    fn a_region_without_a_player_is_dropped() {
        let r = Regions::new([(0, 0), (400, 0)], &[(0, 0)], 0);
        assert_eq!(r.count, 1);
        assert_eq!(r.of_chunk((400, 0)), None);
    }

    #[test]
    fn views_that_touch_merge_and_numbering_is_stable() {
        // Two players 20 chunks apart with a view of 8 chunks: their views reach 4 sections each way and touch.
        let players = [(0, 0), (20, 0)];
        let r = Regions::new([], &players, 8);
        assert_eq!(r.count, 1);
        let again = Regions::new([], &players, 8);
        assert_eq!(again.count, 1);
        assert_eq!(again.of_chunk((-1, 0)), r.of_chunk((-1, 0)));
        assert_eq!(r.of_chunk((-1, 0)), r.of_chunk((0, 0)));
    }

    #[test]
    fn splitting_a_list_keeps_what_no_region_owns() {
        // One region around chunk 0 (blocks -64..128); the entity at x = 1000 stands in no region's chunks.
        let regions = Regions::new([(0, 0)], &[(0, 0)], 0);
        struct TestEntity {
            id: u32,
            x: f64,
            join: horizon_client::join_seq::JoinSeq,
        }
        horizon_client::impl_joined!(TestEntity);
        let mut mobs: JoinList<_> = [(1, 5.0), (2, 1000.0), (3, 20.0)]
            .into_iter()
            .map(|(id, x)| TestEntity {
                id,
                x,
                join: Default::default(),
            })
            .collect();
        let lending = Lending {
            layout: &regions,
            held: &Default::default(),
        };
        let parts = split_list(&mut mobs, &lending, |m| (m.x, 0.0));
        assert_eq!(parts.len(), 1);
        let lent: Vec<u32> = parts[0].iter().map(|m| m.id).collect();
        assert_eq!(lent, [1, 3]);
        // The level keeps the far entity: a save of its chunk still writes it.
        let kept: Vec<u32> = mobs.iter().map(|m| m.id).collect();
        assert_eq!(kept, [2]);
        // A chunk the streaming has not finished (chunk 1, x = 16..32) stays with the level, and so does what
        // stands in it.
        let mut mobs: JoinList<_> = parts.into_iter().flatten().collect();
        let held = [(1, 0)].into_iter().collect();
        let lending = Lending {
            layout: &regions,
            held: &held,
        };
        let parts = split_list(&mut mobs, &lending, |m| (m.x, 0.0));
        let lent: Vec<u32> = parts[0].iter().map(|m| m.id).collect();
        assert_eq!(lent, [1]);
        let kept: Vec<u32> = mobs.iter().map(|m| m.id).collect();
        assert_eq!(kept, [3]);
    }

    #[test]
    fn a_regions_area_says_which_sections_tick_which_are_view_and_which_are_ring() {
        // One player with a view of 8 chunks: the ticking section under them and the view sections around it.
        let r = Regions::new([(0, 0)], &[(0, 0)], 8);
        let area = r.area_of(0);
        assert_eq!(area.get(&(0, 0)), Some(&Kind::Ticking));
        assert_eq!(area.get(&(2, 2)), Some(&Kind::View));
        assert_eq!(area.get(&(1, 0)), Some(&Kind::View));
        // With no view of their own, the ticking section is owned with the ring of one section around it.
        let r = Regions::new([(0, 0)], &[(0, 0)], 0);
        let area = r.area_of(0);
        assert_eq!(area.len(), 9);
        assert_eq!(area.values().filter(|&&k| k == Kind::Ticking).count(), 1);
        assert_eq!(area.values().filter(|&&k| k == Kind::Ring).count(), 8);
    }

    #[test]
    fn players_who_walk_apart_are_separate_groups_and_the_region_keeps_the_one_it_chooses() {
        // The same ticking chunks and two players: together while they stand 6 chunks apart, two groups at 40.
        let together = Regions::new([(0, 0), (6, 0)], &[(0, 0), (6, 0)], 3);
        assert_eq!(together.count, 1);
        let apart = Regions::new([(0, 0), (40, 0)], &[(0, 0), (40, 0)], 3);
        assert_eq!(apart.count, 2);
        // Each group's area is disjoint from the other's: what a split hands over is exactly one group's.
        let (a, b) = (apart.area_of(0), apart.area_of(1));
        assert!(a.keys().all(|s| !b.contains_key(s)));
        // A region carved out of another owns exactly the area it was given, as one region.
        let carved = Regions::for_area(&b, 9 << 20);
        assert_eq!(carved.parts(), 1);
        assert_eq!(carved.of_chunk((40, 0)), Some(0));
        assert_eq!(carved.of_chunk((0, 0)), None);
        assert_eq!(carved.chunk_keys(0).len(), b.len() * 16);
    }

    /// A region game holding two players far apart, each with chunks around them, a cow and an item, claiming
    /// the union of their areas as one region: what a region looks like when its players have walked apart.
    /// `test` gets that game and a spare game to split into.
    fn region_of_two_far_players(
        test: impl FnOnce(&mut Game<'_>, Box<Game<'static>>, Arc<SetShared>, Arc<Spares>),
    ) {
        use horizon_core::geometry::Vec3;
        crate::game::native_guest_turn_tests::with_guests(|game| {
            let far = 4800.0;
            game.lan
                .as_mut()
                .unwrap()
                .hub
                .update_guest(13, |g| {
                    g.position = [far, 64.0, 5.0];
                    g.health = 7.5;
                })
                .unwrap();
            for (cx, cz) in [(0, 0), (300, 0)] {
                for dx in -1..=1 {
                    for dz in -1..=1 {
                        game.world.chunks.insert(
                            (cx + dx, cz + dz),
                            Arc::new(horizon_client::chunk::Chunk::empty(cx + dx, cz + dz)),
                        );
                    }
                }
            }
            let cow = game.res.mob_types().get("minecraft:cow").unwrap().clone();
            let mut near = Mob::new(101, cow.clone(), Vec3::new(3.5, 64.0, 5.5), 0.0);
            near.health = 4.0;
            let mut away = Mob::new(202, cow, Vec3::new(far + 0.5, 64.0, 5.5), 0.0);
            away.health = 3.0;
            game.mobs.push(near);
            game.mobs.push(away);
            game.persist.uuids.insert(202, [1, 2, 3, 4]);
            // The spare a region splits into: made as the server's thread makes them.
            let res = game.static_resources();
            let spare = Box::new(
                super::super::integrated::server_game(
                    res,
                    game.pack.clone(),
                    game.options.clone(),
                    game.settings.clone(),
                )
                .unwrap(),
            );
            // The players' areas as one region claimed them.
            let ticking: Vec<(i32, i32)> = game.world.chunks.keys().copied().collect();
            let cut = Regions::new(
                ticking,
                &[(0, 0), (300, 0)],
                game.lan.as_ref().unwrap().hub.config.view_distance + 1,
            );
            assert_eq!(cut.count, 2);
            let shared = Arc::new(SetShared::default());
            let spares = Arc::new(Spares::default());
            let mut area = cut.area_of(0);
            area.extend(cut.area_of(1));
            for (&section, &kind) in &area {
                assert_eq!(shared.map.claim(7, section, kind), Claim::Held);
            }
            game.regions.area = area;
            game.regions.worker = true;
            game.regions.link = Some(RegionLink {
                shared: Arc::clone(&shared),
                id: 7,
                spares: Arc::clone(&spares),
                ids: Arc::new(AtomicU64::new(7)),
                pool: Arc::new(AtomicU32::new(1_000_000)),
            });
            test(game, spare, shared, spares);
        });
    }

    fn mob_ids(game: &Game<'_>) -> Vec<u32> {
        let mut ids: Vec<u32> = game.mobs.iter().map(|m| m.id).collect();
        ids.sort_unstable();
        ids
    }

    fn guest_conns(game: &Game<'_>) -> Vec<u32> {
        let mut conns: Vec<u32> = game.lan.as_ref().map_or(Vec::new(), |h| {
            h.hub.guests.iter().map(|g| g.conn).collect()
        });
        conns.sort_unstable();
        conns
    }

    #[test]
    #[ignore = "daily"]
    fn a_region_whose_players_walked_apart_splits_itself_and_each_part_keeps_its_state() {
        region_of_two_far_players(|a, spare, shared, spares| {
            spares.give(spare);
            let report = a.region_topology();
            assert_eq!(report.born.len(), 1, "one new region for the far player");
            let mut born = report.born.into_iter().next().unwrap();
            // The player with more... both have one: the first group keeps, the far one is carved out.
            assert_eq!(born.conns, [13]);
            assert_eq!(report.conns, Some(vec![12]));
            let b = &mut born.game;
            // Entities and players: each in the region that owns their place, with what they held.
            assert_eq!(mob_ids(a), [101]);
            assert_eq!(mob_ids(b), [202]);
            assert_eq!(b.mobs[0].health, 3.0);
            assert_eq!(b.persist.uuids.get(&202), Some(&[1, 2, 3, 4]));
            assert_eq!(guest_conns(a), [12]);
            assert_eq!(guest_conns(b), [13]);
            let moved = b.lan.as_ref().unwrap().hub.guest(13).unwrap();
            assert_eq!((moved.health, moved.position[0]), (7.5, 4800.0));
            // Chunks: the 3 x 3 around each player, none shared.
            assert_eq!(a.world.chunks.len(), 9);
            assert_eq!(b.world.chunks.len(), 9);
            assert!(b.world.chunks.contains_key(&(300, 0)));
            // The sections: the far ones are the new region's in the map, and the region's own area lost them.
            let link = a.regions.link.clone().unwrap();
            assert_eq!(
                shared.map.owner_of_chunk((300, 0)).map(|o| o.id),
                Some(born.id)
            );
            assert_eq!(
                shared.map.owner_of_chunk((0, 0)).map(|o| o.id),
                Some(link.id)
            );
            assert!(
                a.regions
                    .area
                    .keys()
                    .all(|s| shared.map.owner(*s).map(|o| o.id) == Some(link.id))
            );
            assert!(
                b.regions
                    .area
                    .keys()
                    .all(|s| shared.map.owner(*s).map(|o| o.id) == Some(born.id))
            );
            // Both are regions that stand on their own: nobody asks for anything more.
            assert!(report.wants.is_none());
            assert_eq!(*shared.merges.lock().unwrap(), Default::default());
        });
    }

    #[test]
    #[ignore = "daily"]
    fn a_region_that_finds_no_spare_game_keeps_everything_and_asks_for_one() {
        region_of_two_far_players(|a, _spare, _shared, spares| {
            let report = a.region_topology();
            assert!(report.born.is_empty());
            assert_eq!(spares.wanted.load(Ordering::Relaxed), 1);
            // Nothing moved, and the next tick works it out again.
            assert_eq!(mob_ids(a), [101, 202]);
            assert_eq!(guest_conns(a), [12, 13]);
            assert_eq!(a.regions.topology_seen, None);
        });
    }

    #[test]
    #[ignore = "daily"]
    fn a_region_absorbed_by_another_hands_over_its_entities_players_chunks_and_sections() {
        region_of_two_far_players(|a, spare, shared, spares| {
            spares.give(spare);
            let mut born = a.region_topology().born.into_iter().next().unwrap();
            let link = a.regions.link.clone().unwrap();
            let whole = a.regions.area.len() + born.game.regions.area.len();
            // The region that split off is within the merge radius of the first again: the one asks the other,
            // and the server's thread merges at the tick boundary (`merge_regions`).
            shared.map.reassign(born.id, link.id);
            a.absorb_region(&mut born.game);
            assert_eq!(mob_ids(a), [101, 202]);
            assert_eq!(a.mobs.iter().find(|m| m.id == 202).unwrap().health, 3.0);
            assert_eq!(a.persist.uuids.get(&202), Some(&[1, 2, 3, 4]));
            assert_eq!(guest_conns(a), [12, 13]);
            let moved = a.lan.as_ref().unwrap().hub.guest(13).unwrap();
            assert_eq!((moved.health, moved.position[0]), (7.5, 4800.0));
            assert_eq!(a.world.chunks.len(), 18);
            assert_eq!(a.regions.area.len(), whole);
            // What was absorbed holds nothing any more.
            assert!(born.game.mobs.is_empty());
            assert!(born.game.world.chunks.is_empty());
            assert!(guest_conns(&born.game).is_empty());
            assert!(born.game.persist.uuids.is_empty());
        });
    }

    #[test]
    #[ignore = "daily"]
    fn a_player_teleported_onto_another_regions_ground_is_handed_to_that_region_by_a_task() {
        use super::super::region_tasks::Arrival;
        region_of_two_far_players(|a, spare, _shared, spares| {
            spares.give(spare);
            let born = a.region_topology().born.into_iter().next().unwrap();
            let mut b = born.game;
            assert!(
                Arc::ptr_eq(&a.regions.tasks, &b.regions.tasks),
                "one set of queues for the server"
            );
            let arrival = Arrival {
                dim: a.dim,
                position: [4800.5, 70.0, 5.5],
                rotation: Some([90.0, 10.0]),
            };
            // The command that teleports player 12 queues a task for them; their region runs it at its tick.
            a.regions.tasks.entity(
                12,
                move |owner| owner.teleport_player_task(12, arrival),
                None,
            );
            assert_eq!(a.regions.tasks.entity_len(12), 1);
            b.run_region_tasks();
            assert_eq!(
                a.regions.tasks.entity_len(12),
                1,
                "the region that does not hold them leaves it"
            );
            a.run_region_tasks();
            // They left A's game with what is kept for them and wait in the queue of the region that owns the
            // place; B takes them in at its own next tick.
            assert_eq!(guest_conns(a), Vec::<u32>::new());
            assert!(a.regions.conns_dirty);
            assert_eq!(a.regions.tasks.region_len(born.id), 1);
            // A task for the player queued meanwhile follows them: B runs it, not A.
            let ran = Arc::new(AtomicU32::new(0));
            let seen = Arc::clone(&ran);
            a.regions.tasks.entity(
                12,
                move |_| {
                    seen.fetch_add(1, Ordering::Relaxed);
                },
                None,
            );
            a.run_region_tasks();
            assert_eq!(ran.load(Ordering::Relaxed), 0);
            b.run_region_tasks();
            assert_eq!(guest_conns(&b), [12, 13]);
            assert_eq!(
                ran.load(Ordering::Relaxed),
                1,
                "their new region ran the task queued for them"
            );
            let g = b.lan.as_ref().unwrap().hub.guest(12).unwrap();
            assert_eq!(g.position, [4800.5, 70.0, 5.5]);
            assert_eq!((g.yaw, g.pitch), (90.0, 10.0));
            assert!(b.regions.conns_dirty);
        });
    }

    #[test]
    #[ignore = "daily"]
    fn a_region_asks_the_region_of_a_player_for_their_place_and_gets_it_a_tick_later() {
        use super::super::region_tasks::Answer;
        region_of_two_far_players(|a, spare, _shared, spares| {
            spares.give(spare);
            let born = a.region_topology().born.into_iter().next().unwrap();
            let mut b = born.game;
            assert!(matches!(a.place_of_remote(13), Answer::Asked));
            assert!(
                matches!(a.place_of_remote(13), Answer::Asked),
                "asked once, still waiting"
            );
            assert_eq!(a.regions.tasks.entity_len(13), 1);
            // The player's own region answers at the start of its tick.
            b.run_region_tasks();
            let Answer::Ready(place) = a.place_of_remote(13) else {
                panic!("answered");
            };
            assert_eq!(place.name, "guest13");
            assert_eq!(place.position, [4800.0, 64.0, 5.0]);
            assert_eq!(place.dim, a.dim);
            // The answer is used up: a later question is a new one.
            assert!(matches!(a.place_of_remote(13), Answer::Asked));
        });
    }
    #[test]
    #[ignore = "daily"]
    fn time_weather_and_gamerule_run_in_the_region_of_the_player_who_typed_them() {
        use horizon_protocol::packets::play::{ChatCommand, Serverbound};
        crate::game::native_guest_turn_tests::with_guests(|game| {
            let hub = &game.lan.as_ref().unwrap().hub;
            let typed = |line: &str| {
                Serverbound::ChatCommand(ChatCommand {
                    command: line.to_owned(),
                })
            };
            for line in [
                "time set day",
                "/time add 100",
                "weather rain 600",
                "gamerule keep_inventory true",
                "time query daytime",
            ] {
                assert!(hub.away_local(&typed(line)), "{line} stays in its region");
            }
            // A command that merely starts like one, or reaches players, brings every region home.
            for line in [
                "timer",
                "execute as @a run time set day",
                "kill @a",
                "give Bob stone",
            ] {
                assert!(!hub.away_local(&typed(line)), "{line} lands the regions");
            }
        });
    }

    #[test]
    #[ignore = "daily"]
    fn a_regions_time_weather_and_rules_are_the_global_ticks_and_its_changes_reach_every_region() {
        use super::super::global_tick::Change;
        region_of_two_far_players(|a, mut home, _shared, _spares| {
            home.regions.worker = false;
            home.regions.global = Arc::clone(&a.regions.global);
            home.regions.tasks = Arc::clone(&a.regions.tasks);
            home.dim = a.dim;
            // The global tick publishes the level: the region reads it at the start of its tick.
            home.world.day_time = 1000;
            home.world.game_time = 5000;
            home.publish_global();
            a.read_global();
            assert_eq!((a.world.day_time, a.world.game_time), (1000, 5000));
            // The region's /weather and /gamerule change its own copy at once and tell the global tick.
            let rain = Change::Weather {
                clear: 0,
                time: 600,
                raining: true,
                thundering: false,
            };
            rain.apply(a);
            a.global_change(rain);
            let rule = Change::Rule {
                name: "minecraft:keep_inventory".to_owned(),
                value: horizon_client::gamerules::Value::Bool(true),
            };
            rule.apply(a);
            a.global_change(rule);
            assert!(a.world.weather.raining);
            assert!(
                !home.world.weather.raining,
                "the level has not heard of it yet"
            );
            // The global tick takes the messages in, applies them and publishes: the change is acknowledged.
            home.tick_global();
            home.publish_global();
            assert!(home.world.weather.raining);
            assert_eq!(home.world.weather.rain_time, 600);
            assert!(
                home.ui
                    .rules
                    .bool_at(horizon_client::gamerules::vanilla::KEEP_INVENTORY)
            );
            // The region read what it sent back, and has nothing left to apply again.
            a.read_global();
            assert!(a.regions.unacked.is_empty());
            assert!(a.world.weather.raining);
            // Another region that never sent anything reads the same level at its next tick.
            let view = home.regions.global.snapshot(home.dim).unwrap();
            assert!(view.weather.raining);
            assert!(
                view.rules
                    .bool_at(horizon_client::gamerules::vanilla::KEEP_INVENTORY)
            );
        });
    }

    #[test]
    #[ignore = "daily"]
    fn a_level_wide_spawn_is_granted_to_one_region_and_run_there_once() {
        use super::super::global_tick::{Census, SpawnerKind};
        region_of_two_far_players(|a, mut home, _shared, _spares| {
            home.regions.worker = false;
            home.regions.global = Arc::clone(&a.regions.global);
            home.regions.tasks = Arc::clone(&a.regions.tasks);
            home.dim = a.dim;
            // The global tick knows the region (7) holds the only living players.
            home.regions.gstate.record(
                home.dim,
                7,
                Census {
                    alive: 2,
                    ..Census::default()
                },
                0,
            );
            assert_eq!(
                home.grant_spawn(SpawnerKind::Cat),
                None,
                "sent, not run here"
            );
            assert_eq!(a.regions.tasks.region_len(7), 1);
            assert_eq!(a.regions.global.open_grants(), 1);
            // The region claims it at the start of its next tick, and it is used up.
            a.run_region_tasks();
            assert_eq!(a.regions.global.open_grants(), 0);
            assert_eq!(a.regions.tasks.region_len(7), 0);
        });
    }
}
