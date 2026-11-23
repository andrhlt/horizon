//! Who owns each section of a level, readable by every region thread.
//!
//! A level is cut into sections (`regions.rs`); a region owns the sections it has players, ticking chunks or
//! view in, and a ring of one section around each ticking one. [`SectionMap`] holds that ownership for the whole
//! level, the same idea as Folia's `ThreadedRegionizer`: a table of sections, each with an owner, which any
//! region thread reads and claims from without the server thread. The table is cut into shards, each with a
//! lock of its own, so the regions' readers and claimers rarely meet.
//!
//! A region keeps the sections it has claimed as an [`Area`]. At the end of its tick it works out the area its
//! players and ticking chunks need now, claims what it lacks, and finds out which other regions are within the
//! merge radius of it ([`SectionMap::neighbours`]): those two regions merge at their next tick boundary.
use super::regions::MERGE_RADIUS;
use horizon_core::fast_hash::{FxHashMap, FxHashSet};
use std::sync::RwLock;

/// A section's key: its chunk coordinates shifted by [`super::regions::SECTION_SHIFT`].
pub(super) type Section = (i32, i32);

/// What a section is to its region. The order is the strength of the claim: a region upgrades its own section
/// to a stronger kind, never the other way.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Kind {
    /// Owned only because it is next to a ticking section: a region's ticks find the chunks next to theirs.
    Ring,
    /// Inside a player's view distance, and nothing more.
    View,
    /// A chunk in it ticks, or a player stands in it.
    Ticking,
}

/// A region's identity in the map: unique for the server's life, never reused.
pub(super) type RegionId = u64;

/// A section's owner.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Owner {
    pub id: RegionId,
    pub kind: Kind,
}

/// The sections one region owns.
pub(super) type Area = FxHashMap<Section, Kind>;

/// The result of asking for a section.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Claim {
    /// The section is the claimer's now (or was).
    Held,
    /// Another region owns it.
    Taken(Owner),
}

const SHARDS: usize = 64;

/// The owner of every owned section of one level.
pub(super) struct SectionMap {
    shards: Vec<RwLock<FxHashMap<Section, Owner>>>,
}

impl Default for SectionMap {
    fn default() -> Self {
        Self {
            shards: (0..SHARDS).map(|_| RwLock::default()).collect(),
        }
    }
}

fn read<T>(l: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    l.read().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn write<T>(l: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    l.write().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Whether two sections of these kinds, `apart` sections from each other at most (either axis), belong to one
/// region: ticking sections `MERGE_RADIUS` apart or closer, and any other pair that touch.
pub(super) fn joins(a: Kind, b: Kind, apart: i32) -> bool {
    if a == Kind::Ticking && b == Kind::Ticking {
        apart <= MERGE_RADIUS
    } else {
        apart <= 1
    }
}

impl SectionMap {
    fn shard(&self, (x, z): Section) -> &RwLock<FxHashMap<Section, Owner>> {
        let h = (x as u32).wrapping_mul(0x9E37_79B1) ^ (z as u32).wrapping_mul(0x85EB_CA6B);
        &self.shards[((h >> 8) as usize) % SHARDS]
    }

    /// Who owns `section`.
    pub fn owner(&self, section: Section) -> Option<Owner> {
        read(self.shard(section)).get(&section).copied()
    }

    /// Who owns chunk `(x, z)`: any region thread asks this about a chunk next to its own.
    pub fn owner_of_chunk(&self, (x, z): (i32, i32)) -> Option<Owner> {
        self.owner((
            x >> super::regions::SECTION_SHIFT,
            z >> super::regions::SECTION_SHIFT,
        ))
    }

    /// Takes `section` for `id` as a `kind`, if no other region owns it. Atomic: of two regions that ask for the
    /// same section at once, one gets it and the other is told who did.
    pub fn claim(&self, id: RegionId, section: Section, kind: Kind) -> Claim {
        let mut shard = write(self.shard(section));
        match shard.get_mut(&section) {
            None => {
                shard.insert(section, Owner { id, kind });
                Claim::Held
            }
            Some(o) if o.id == id => {
                o.kind = o.kind.max(kind);
                Claim::Held
            }
            Some(o) => Claim::Taken(*o),
        }
    }

    /// `section` goes from `from` to `to`, if `from` owns it: a region split off another takes its part of the
    /// sections over without any other region getting in between.
    pub fn hand(&self, from: RegionId, to: RegionId, section: Section, kind: Kind) -> bool {
        let mut shard = write(self.shard(section));
        match shard.get_mut(&section) {
            Some(o) if o.id == from => {
                *o = Owner { id: to, kind };
                true
            }
            _ => false,
        }
    }

    /// Gives up `section` if `id` owns it.
    pub fn release(&self, id: RegionId, section: Section) {
        let mut shard = write(self.shard(section));
        if shard.get(&section).is_some_and(|o| o.id == id) {
            shard.remove(&section);
        }
    }

    /// Everything `id` owns, taken out of the map (a region that is gone, or the region a merge absorbed).
    pub fn release_all(&self, id: RegionId) {
        for shard in &self.shards {
            write(shard).retain(|_, o| o.id != id);
        }
    }

    /// The sections `from` owns are `to`'s now: a merge.
    pub fn reassign(&self, from: RegionId, to: RegionId) {
        for shard in &self.shards {
            for o in write(shard).values_mut() {
                if o.id == from {
                    o.id = to;
                }
            }
        }
    }

    /// The sections `id` owns, with their kinds.
    #[cfg(test)]
    pub fn area_of(&self, id: RegionId) -> Area {
        let mut out = Area::default();
        for shard in &self.shards {
            out.extend(
                read(shard)
                    .iter()
                    .filter(|(_, o)| o.id == id)
                    .map(|(s, o)| (*s, o.kind)),
            );
        }
        out
    }

    /// How many sections are owned.
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.shards.iter().map(|s| read(s).len()).sum()
    }

    /// The regions other than `id` that have to merge with a region that owns `area` ([`joins`]): the ones
    /// owning a section within the merge radius of a ticking section of the area, or next to one of its kinds.
    pub fn neighbours(&self, id: RegionId, area: &Area) -> FxHashSet<RegionId> {
        let mut out = FxHashSet::default();
        for (&(x, z), &mine) in area {
            if mine == Kind::Ring {
                continue;
            }
            let reach = if mine == Kind::Ticking {
                MERGE_RADIUS
            } else {
                1
            };
            for dx in -reach..=reach {
                for dz in -reach..=reach {
                    let Some(o) = self.owner((x + dx, z + dz)) else {
                        continue;
                    };
                    if o.id != id
                        && o.kind != Kind::Ring
                        && joins(mine, o.kind, dx.abs().max(dz.abs()))
                    {
                        out.insert(o.id);
                    }
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area(ticking: &[Section]) -> Area {
        let mut a = Area::default();
        for &(x, z) in ticking {
            for dx in -1..=1 {
                for dz in -1..=1 {
                    a.entry((x + dx, z + dz)).or_insert(Kind::Ring);
                }
            }
            a.insert((x, z), Kind::Ticking);
        }
        a
    }

    fn install(map: &SectionMap, id: RegionId, a: &Area) {
        for (&s, &k) in a {
            assert_eq!(map.claim(id, s, k), Claim::Held);
        }
    }

    #[test]
    fn a_section_goes_to_the_region_that_claims_it_first() {
        let map = SectionMap::default();
        assert_eq!(map.claim(1, (4, 4), Kind::View), Claim::Held);
        assert_eq!(
            map.claim(2, (4, 4), Kind::Ticking),
            Claim::Taken(Owner {
                id: 1,
                kind: Kind::View
            })
        );
        // The owner upgrades its own section and never weakens it.
        assert_eq!(map.claim(1, (4, 4), Kind::Ticking), Claim::Held);
        assert_eq!(map.claim(1, (4, 4), Kind::Ring), Claim::Held);
        assert_eq!(map.owner((4, 4)).map(|o| o.kind), Some(Kind::Ticking));
        map.release(2, (4, 4));
        assert_eq!(map.owner((4, 4)).map(|o| o.id), Some(1));
        map.release(1, (4, 4));
        assert_eq!(map.owner((4, 4)), None);
    }

    #[test]
    fn of_many_threads_claiming_one_section_exactly_one_holds_it() {
        let map = std::sync::Arc::new(SectionMap::default());
        let winners: Vec<RegionId> = (1..=16u64)
            .map(|id| {
                let map = std::sync::Arc::clone(&map);
                std::thread::spawn(move || {
                    (map.claim(id, (7, -3), Kind::Ticking) == Claim::Held).then_some(id)
                })
            })
            .collect::<Vec<_>>()
            .into_iter()
            .filter_map(|t| t.join().unwrap())
            .collect();
        assert_eq!(winners.len(), 1);
        assert_eq!(map.owner((7, -3)).map(|o| o.id), Some(winners[0]));
    }

    #[test]
    fn a_region_reads_who_owns_the_chunk_next_to_its_own() {
        let map = SectionMap::default();
        install(&map, 1, &area(&[(0, 0)]));
        // Sections are 4 chunks wide: chunks -4..=7 are the ticking section 0 and its ring.
        assert_eq!(map.owner_of_chunk((7, -4)).map(|o| o.id), Some(1));
        assert_eq!(map.owner_of_chunk((8, 0)), None);
        assert_eq!(map.owner_of_chunk((-5, 0)), None);
    }

    #[test]
    fn regions_within_the_merge_radius_are_neighbours_and_three_apart_are_not() {
        let map = SectionMap::default();
        let first = area(&[(0, 0)]);
        install(&map, 1, &first);
        // Ticking sections three apart own disjoint rings and stay separate.
        let far = area(&[(3, 0)]);
        install(&map, 2, &far);
        assert!(map.neighbours(2, &far).is_empty());
        assert!(map.neighbours(1, &first).is_empty());
        // Two apart: the rings meet. The newcomer is refused the sections they share, and the radius names the
        // owner from both sides.
        let close = area(&[(0, 2)]);
        let refused: Vec<_> = close
            .iter()
            .filter_map(|(&s, &k)| match map.claim(4, s, k) {
                Claim::Taken(o) => Some(o.id),
                Claim::Held => None,
            })
            .collect();
        assert!(refused.contains(&1));
        assert!(map.neighbours(4, &close).contains(&1));
        assert!(map.neighbours(1, &first).contains(&4));
    }

    #[test]
    fn views_that_touch_are_neighbours_and_views_a_section_apart_are_not() {
        let map = SectionMap::default();
        let mut a = Area::default();
        a.insert((0, 0), Kind::View);
        install(&map, 1, &a);
        let mut touching = Area::default();
        touching.insert((1, 1), Kind::View);
        assert!(map.neighbours(2, &touching).contains(&1));
        let mut apart = Area::default();
        apart.insert((2, 0), Kind::View);
        assert!(map.neighbours(2, &apart).is_empty());
    }

    #[test]
    fn a_merge_hands_every_section_to_the_absorber() {
        let map = SectionMap::default();
        install(&map, 1, &area(&[(0, 0)]));
        install(&map, 2, &area(&[(20, 20)]));
        let before = map.len();
        map.reassign(2, 1);
        assert_eq!(map.len(), before);
        assert_eq!(map.area_of(2).len(), 0);
        assert_eq!(map.area_of(1).len(), 18);
        map.release_all(1);
        assert_eq!(map.len(), 0);
    }
}
