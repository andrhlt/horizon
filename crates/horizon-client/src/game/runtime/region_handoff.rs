//! Players that move from one region to another: a teleport within a level, into another level, a portal.
//!
//! A player is never left with a region that does not own the ground they stand on. When one moves (a command,
//! a portal, a mod, a task another region queued for them), the player is taken out of the game that held them
//! with what their host keeps for them ([`PlayerKit`](super::lan::PlayerKit)) and given to the game of the region
//! that owns the place: at the next tick boundary the server thread finds that region in the level's section map
//! and gives the player to it, with no region landed for it. A place no region owns gets a region of its own,
//! made from a spare game. A level without resident regions is held by the server's game: the player goes there.
//!
//! The same step serves every way a player arrives, so a portal's or a command's arrival in a level with regions
//! needs nothing of its own: a player the server thread's game holds in a level that has resident regions is
//! placed in one at the next boundary ([`Game::route_strays`]).
use super::region_map::Claim;
use super::region_tasks::HandOff;
use super::regions::{RegionLink, Regions, ResidentSet, Slot};
use super::{Dim, Game};
use horizon_core::fast_hash::FxHashSet;
use std::sync::Arc;
use std::sync::atomic::Ordering;

/// Region `k`'s game is at rest between two ticks: home, not landed, not still running.
fn at_rest(slot: &mut Slot) -> bool {
    let _ = slot.home(Some(std::time::Instant::now()));
    slot.running.is_none() && !slot.landed && slot.game.is_some()
}

impl Game<'_> {
    /// The players regions handed off for a level that has no resident regions here, or another level than the
    /// one they left: the server thread's game takes them in at the start of its tick, and the teleport they
    /// were handed off for is made there (a level teleport, for a place in another level).
    pub(super) fn deliver_server_handoffs(&mut self) {
        let tasks = Arc::clone(&self.regions.tasks);
        let resident: Vec<Dim> = self.regions.resident.keys().copied().collect();
        let list =
            tasks.take_handoffs(|h| !(h.from == h.arrival.dim && resident.contains(&h.from)));
        for h in list {
            self.take_in_handoff(None, h);
        }
    }

    /// The players regions handed off for a place in this level, which has resident regions `set`: taken in by
    /// the server's game for the arrival, then placed in the region that owns it ([`Game::route_strays`]).
    pub(super) fn deliver_set_handoffs(&mut self, set: &mut ResidentSet) {
        let dim = self.dim;
        let list = self
            .regions
            .tasks
            .take_handoffs(|h| h.from == dim && h.arrival.dim == dim);
        for h in list {
            self.take_in_handoff(Some(set), h);
        }
    }

    /// One handed-off player into the server's game, at `arrival`.
    fn take_in_handoff(&mut self, mut set: Option<&mut ResidentSet>, h: HandOff) {
        let HandOff { kit, from, arrival } = h;
        let conn = kit.guest.conn;
        // Their packets are the server's again until a region has them.
        if let Some(set) = &mut set {
            forget_conn(set, conn);
        }
        self.forget_away_conn(conn);
        let Some(host) = self.lan.as_mut() else {
            return;
        };
        host.give_player_kit(kit, from);
        host.hub.release_away_conns(&[conn]);
        self.bind_lan_residents();
        self.arrive_player(super::commands::Target::Guest(conn), arrival);
    }

    /// `conn` is no longer among the connections of any resident region.
    pub(super) fn forget_away_conn(&mut self, conn: u32) {
        for set in self.regions.resident.values_mut() {
            forget_conn(set, conn);
        }
    }

    /// Places each player the server's game holds in this level, in the region that owns the ground they stand
    /// on: a player who arrived by a command, a portal, a respawn or a hand-off. Whether any is left that no
    /// region could take now (its game still ticks, or no spare game is made yet): the level's regions come home
    /// for it, as before.
    pub(super) fn route_strays(&mut self, set: &mut ResidentSet) -> bool {
        let Some(host) = self.lan.as_ref() else {
            return false;
        };
        let strays: Vec<u32> = host
            .hub
            .guests
            .iter()
            .filter(|g| host.hub.here(g) && !set.games.iter().any(|s| s.conns.contains(&g.conn)))
            .map(|g| g.conn)
            .collect();
        let mut left = false;
        for conn in strays {
            left |= !self.place_in_set(set, conn);
        }
        left
    }

    /// The player on `conn`, held by the server's game, given to the region of `set` that owns their chunk, or
    /// to a new one when none does. False when it could not be done this tick.
    fn place_in_set(&mut self, set: &mut ResidentSet, conn: u32) -> bool {
        let Some(chunk) = self
            .lan
            .as_ref()
            .and_then(|h| h.hub.guest(conn))
            .map(|g| g.chunk())
        else {
            return false;
        };
        let Some(owner) = set.shared.map.owner_of_chunk(chunk) else {
            return self.new_region_for(set, conn);
        };
        let Some(k) = set.games.iter().position(|s| s.uid == owner.id) else {
            return false;
        };
        if !at_rest(&mut set.games[k]) {
            return false;
        }
        let dim = self.dim;
        let Some(mut host) = self.lan.take() else {
            return false;
        };
        let guests = host.hub.lend_guest(conn);
        let slot = &mut set.games[k];
        let Some(w) = slot.game.as_deref_mut() else {
            self.lan = Some(host);
            return false;
        };
        let mut shell = w.lan.take().unwrap_or_else(|| host.region_shell());
        host.lend_region(&mut shell, &[conn], &[], dim, guests);
        w.lan = Some(shell);
        w.bind_lan_residents();
        host.hub.away.insert(conn);
        slot.conns.push(conn);
        // Their view reaches chunks the region does not hold yet: its own streamer loads them.
        self.lan = Some(host);
        self.bind_lan_residents();
        log::info!(
            "regions: player on connection {conn} handed to region {}",
            owner.id
        );
        true
    }

    /// A region of its own, from a spare game, for the player on `conn` (and any other player of the server's
    /// game whose ground it covers): the places no region owns. False while no spare game is ready: the server's
    /// thread makes one meanwhile.
    fn new_region_for(&mut self, set: &mut ResidentSet, conn: u32) -> bool {
        let dim = self.dim;
        let Some(host) = self.lan.as_ref() else {
            return false;
        };
        let Some(chunk) = host.hub.guest(conn).map(|g| g.chunk()) else {
            return false;
        };
        let view = host.hub.config.view_distance.clamp(2, 32) + 1;
        let Some(mut w) = self.regions.spares.take() else {
            return false;
        };
        let levels = self.tickets.levels();
        let ticking = self
            .world
            .chunks
            .keys()
            .copied()
            .filter(|&(x, z)| levels.block_ticking(x, z));
        let cut = Regions::new(ticking, &[chunk], view);
        let id = self.regions.ids.fetch_add(1, Ordering::Relaxed) + 1;
        // What no region owns is claimed; a section another region holds is not, and that region merges with
        // this one at the next boundary.
        let mut area = super::region_map::Area::default();
        let mut neighbours: FxHashSet<super::region_map::RegionId> = FxHashSet::default();
        for (&section, &kind) in &cut.area_of(0) {
            match set.shared.map.claim(id, section, kind) {
                Claim::Held => {
                    area.insert(section, kind);
                }
                Claim::Taken(o) => {
                    neighbours.insert(o.id);
                }
            }
        }
        if area.is_empty() {
            self.regions.spares.give(w);
            return false;
        }
        let layout = Regions::for_area(&area, (id as usize) << 20);
        let Some(mut host) = self.lan.take() else {
            self.regions.spares.give(w);
            return false;
        };
        let conns: Vec<u32> = host
            .hub
            .guests
            .iter()
            .filter(|g| host.hub.here(g) && layout.of_chunk(g.chunk()).is_some())
            .map(|g| g.conn)
            .collect();
        let mut guests = Vec::new();
        for &c in &conns {
            guests.extend(host.hub.lend_guest(c));
        }
        let tracker = host
            .hub
            .lend_trackers(1, |c| layout.of_chunk(c))
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
        let pool = Arc::clone(self.regions.entity_pool.get_or_insert_with(|| {
            Arc::new(std::sync::atomic::AtomicU32::new(self.next_entity + 1))
        }));
        self.lend_region(&mut w, part);
        w.copy_world_settings_from(self);
        w.refill_entity_ids(&pool);
        self.lend_maps(&mut w);
        host.hub.away.extend(conns.iter().copied());
        let mut shell = w.lan.take().unwrap_or_else(|| host.region_shell());
        host.lend_region(&mut shell, &conns, &mob_ids, dim, guests);
        if let Some(tracker) = tracker {
            shell.hub.set_tracker(tracker);
        }
        w.lan = Some(shell);
        w.bind_lan_residents();
        self.lan = Some(host);
        w.regions.area = area;
        w.regions.topology_seen = None;
        w.regions.link = Some(RegionLink {
            shared: Arc::clone(&set.shared),
            id,
            spares: Arc::clone(&self.regions.spares),
            ids: Arc::clone(&self.regions.ids),
            pool,
        });
        if let Some(link) = w.regions.link.clone() {
            w.own_region_streaming(&link);
        }
        for other in neighbours {
            set.shared.ask_merge(id, other);
        }
        self.streaming.lent = None;
        let lent = Arc::make_mut(&mut set.lent);
        lent.extend(chunks.iter().map(|c| (c.x, c.z)));
        let mut slot = Slot::new(w, id);
        slot.conns = conns;
        slot.fresh = true;
        set.games.push(slot);
        self.streaming.lent = Some(Arc::clone(&set.lent));
        log::info!("regions: region {id} made for the player on connection {conn}");
        true
    }
}

/// `conn` is no longer among the connections of any region of `set`.
fn forget_conn(set: &mut ResidentSet, conn: u32) {
    for slot in &mut set.games {
        slot.conns.retain(|&c| c != conn);
    }
}
