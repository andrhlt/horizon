//! Teleports between regions, end to end: a dedicated server by regions, a LAN host with three protocol bots far
//! apart in separate regions (the way `crates/horizon-client/tests/lan_bot.rs` joins one), and a teleport
//! within a level, into another level and out of it. Each is checked from both sides: the position the bot's
//! client was told, and the one the server holds, in the game of the region that owns the player.
use super::*;
use crate::game::commands::Target;
use horizon_protocol::{
    PROTOCOL_VERSION,
    connection::{ConnWriter, Connection},
    crypto::offline_uuid,
    packets::{
        Empty,
        configuration::{self as cfg, ClientInformation},
        handshake, login,
        play::{self, Clientbound as C, Serverbound as S},
    },
};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// What a bot's client has been told so far.
#[derive(Default)]
struct Seen {
    /// The last place the server put it (`ClientboundPlayerPosition`).
    position: Option<[f64; 3]>,
    /// The level it is in, from its login or last respawn.
    dimension: String,
    /// "No entity was found" and the like, as the server told it.
    notfound: u32,
    left: Option<String>,
    /// What the server said in chat, for a failure's report.
    chat: Vec<String>,
    /// The chunks the server sent and has not told it to forget.
    chunks: std::collections::HashSet<(i32, i32)>,
}

thread_local! {
    /// The bots of the running test, for the report of a timeout.
    static BOTS: std::cell::RefCell<Vec<Arc<BotSeen>>> = const { std::cell::RefCell::new(Vec::new()) };
}

struct BotSeen {
    name: &'static str,
    seen: Arc<Mutex<Seen>>,
}

/// A protocol bot: joined, answering what a client must (teleports, chunk batches, keep-alives).
struct Bot {
    name: &'static str,
    writer: Arc<Mutex<ConnWriter>>,
    seen: Arc<Mutex<Seen>>,
}

impl Bot {
    fn connect(port: u16, name: &'static str) -> std::result::Result<Bot, String> {
        let e = |e: horizon_protocol::Error| e.to_string();
        let mut c = Connection::connect("127.0.0.1", port, Duration::from_secs(5)).map_err(e)?;
        c.reader
            .set_read_timeout(Some(Duration::from_secs(120)))
            .map_err(e)?;
        c.send(
            &handshake::Serverbound::Intention(handshake::Intention {
                protocol: PROTOCOL_VERSION,
                host: "127.0.0.1".into(),
                port,
                intent: handshake::LOGIN,
            })
            .encode(),
        )
        .map_err(e)?;
        c.send(
            &login::Serverbound::Hello(login::ServerHello {
                name: name.into(),
                uuid: offline_uuid(name),
            })
            .encode(),
        )
        .map_err(e)?;
        loop {
            let (id, payload) = c.recv().map_err(e)?;
            match login::Clientbound::decode(id, &payload).map_err(e)? {
                login::Clientbound::Compression(p) => c.set_compression(p.threshold),
                login::Clientbound::LoginFinished(_) => {
                    c.send(&login::Serverbound::LoginAcknowledged(Empty).encode())
                        .map_err(e)?;
                    break;
                }
                login::Clientbound::Disconnect(d) => return Err(d.reason_json),
                other => return Err(format!("unexpected login packet {other:?}")),
            }
        }
        c.send(&cfg::Serverbound::ClientInformation(ClientInformation::new(2)).encode())
            .map_err(e)?;
        loop {
            let (id, payload) = c.recv().map_err(e)?;
            match cfg::Clientbound::decode(id, &payload).map_err(e)? {
                cfg::Clientbound::SelectKnownPacks(p) => {
                    c.send(
                        &cfg::Serverbound::SelectKnownPacks(cfg::SelectKnownPacks {
                            packs: p.packs,
                        })
                        .encode(),
                    )
                    .map_err(e)?;
                }
                cfg::Clientbound::FinishConfiguration(_) => {
                    c.send(&cfg::Serverbound::FinishConfiguration(Empty).encode())
                        .map_err(e)?;
                    break;
                }
                _ => {}
            }
        }
        let (mut reader, writer) = c.split();
        let writer = Arc::new(Mutex::new(writer));
        let seen = Arc::new(Mutex::new(Seen::default()));
        let (w, s) = (Arc::clone(&writer), Arc::clone(&seen));
        std::thread::spawn(move || {
            let send = |p: S| {
                let _ = w.lock().unwrap().send(&p.encode());
            };
            while let Ok((id, payload)) = reader.recv() {
                // The command tree has argument nodes the client side does not decode.
                let Ok(packet) = C::decode(id, &payload) else {
                    continue;
                };
                match packet {
                    C::Login(l) => s.lock().unwrap().dimension = l.spawn.dimension,
                    C::Respawn(r) => s.lock().unwrap().dimension = r.spawn.dimension,
                    C::PlayerPosition(p) => {
                        s.lock().unwrap().position = Some(p.position);
                        send(S::AcceptTeleportation(play::AcceptTeleportation {
                            id: p.teleport,
                            x: p.position[0],
                            y: p.position[1],
                            z: p.position[2],
                            yaw: p.yaw,
                            pitch: p.pitch,
                        }));
                    }
                    C::LevelChunkWithLight(c) => {
                        s.lock().unwrap().chunks.insert((c.x, c.z));
                    }
                    C::ForgetLevelChunk(c) => {
                        s.lock().unwrap().chunks.remove(&(c.pos[0], c.pos[1]));
                    }
                    C::ChunkBatchFinished(_) => {
                        send(S::ChunkBatchReceived(play::ChunkBatchReceived {
                            chunks_per_tick: 20.0,
                        }));
                        send(S::PlayerLoaded(Empty));
                    }
                    C::KeepAlive(k) => send(S::KeepAlive(k)),
                    C::Ping(p) => send(S::Pong(p)),
                    C::SystemChat(chat) => {
                        let text = format!("{:?}", chat.content);
                        let mut seen = s.lock().unwrap();
                        if text.contains("argument.entity.notfound") {
                            seen.notfound += 1;
                        }
                        seen.chat.push(readable(&text));
                    }
                    C::Disconnect(d) => {
                        s.lock().unwrap().left = Some(format!("{d:?}"));
                        return;
                    }
                    _ => {}
                }
            }
        });
        Ok(Bot { name, writer, seen })
    }

    /// Types `line` (a command without its slash).
    fn command(&self, line: &str) {
        let body = S::ChatCommand(play::ChatCommand {
            command: line.to_owned(),
        })
        .encode();
        self.writer.lock().unwrap().send(&body).unwrap();
    }

    /// Says it stands at `(x, y, z)`: a step of a walk (the server's movement check allows a flying player this).
    fn step_to(&self, x: f64, y: f64, z: f64) {
        let body = S::MovePlayerPos(play::MovePlayerPos {
            x,
            y,
            z,
            flags: 1,
        })
        .encode();
        let mut writer = self.writer.lock().unwrap();
        let _ = writer.send(&body);
        // A client ends each of its ticks: the server counts one position a tick.
        let _ = writer.send(&S::ClientTickEnd(Empty).encode());
    }

    fn position(&self) -> Option<[f64; 3]> {
        self.seen.lock().unwrap().position
    }

    fn dimension(&self) -> String {
        self.seen.lock().unwrap().dimension.clone()
    }
}

/// Where the server holds a player: its place and level, and the region of the level that has them (None: the
/// server thread's own game).
#[derive(Debug)]
struct Held {
    position: [f64; 3],
    dimension: String,
    region: Option<u64>,
}

/// The player `name` wherever the server holds them: its own game, or a resident region's that is at rest.
fn find(game: &mut Game<'_>, name: &str) -> Option<Held> {
    if let Some(g) = game
        .lan
        .as_ref()?
        .hub
        .guests
        .iter()
        .find(|g| g.name == name && g.placed)
    {
        return Some(Held {
            position: g.position,
            dimension: g.dimension.clone(),
            region: None,
        });
    }
    for set in game.regions.resident.values_mut() {
        for slot in &mut set.games {
            let _ = slot.home(Some(Instant::now()));
            let Some(w) = slot.game.as_deref() else {
                continue;
            };
            let Some(g) = w
                .lan
                .as_ref()
                .and_then(|h| h.hub.guests.iter().find(|g| g.name == name && g.placed))
            else {
                continue;
            };
            return Some(Held {
                position: g.position,
                dimension: g.dimension.clone(),
                region: Some(slot.uid),
            });
        }
    }
    None
}

/// How many resident regions the Overworld has.
fn regions_of(game: &Game<'_>, dim: Dim) -> usize {
    game.regions.resident.get(&dim).map_or(0, |s| s.games.len())
}

/// Ticks the server until `done` (at most `secs` seconds), as the game loop would, with the bots' threads
/// running meanwhile.
fn until(game: &mut Game<'_>, what: &str, secs: u64, mut done: impl FnMut(&mut Game<'_>) -> bool) {
    let bots = BOTS.with(|b| b.borrow().clone());
    let deadline = Instant::now() + Duration::from_secs(secs);
    while !done(game) {
        if Instant::now() >= deadline {
            let held: Vec<_> = ["Alpha", "Bravo", "Charlie"]
                .iter()
                .map(|n| (*n, find(game, n)))
                .collect();
            let host = game.lan.as_ref().unwrap();
            let slots: Vec<_> = game
                .regions
                .resident
                .iter()
                .map(|(d, s)| {
                    (
                        d.id(),
                        s.games
                            .iter()
                            .map(|x| (x.uid, x.conns.clone(), x.landed, x.running.is_some()))
                            .collect::<Vec<_>>(),
                    )
                })
                .collect();
            let report = format!(
                "timed out waiting for {what}: {held:#?}\nslots {slots:?}\ndeferred {} commands {} questions {} away {:?}",
                host.deferred_commands.len(),
                host.commands.len(),
                game.regions.questions.len(),
                host.hub.away
            ) + &bots
                .iter()
                .map(|b| format!("\n{}: {:?}", b.name, b.seen.lock().unwrap().chat))
                .collect::<String>();
            panic!("{report}");
        }
        game.tick_server(false).unwrap();
        std::thread::sleep(Duration::from_millis(15));
    }
}

fn is(position: Option<[f64; 3]>, want: [f64; 3]) -> bool {
    position.is_some_and(|p| p.iter().zip(want).all(|(a, b)| (a - b).abs() < 1e-6))
}

fn open_flat<'a>(
    resources: &'a mut Option<Arc<Resources>>,
    root: &std::path::Path,
    home: &std::path::Path,
) -> Game<'a> {
    use crate::game::{Audio, Options, Progress, Settings, load_resources};
    let settings = Settings::default();
    let loaded = load_resources(
        horizon_assets::resource_source::ResourceRoot::directory(root.to_path_buf()),
        Vec::new(),
        None,
        root,
        &settings,
        Instant::now(),
        false,
        crate::game::execution_role::ExecutionRole::IntegratedServer,
        &Progress::new(),
        &mut |_, _| unreachable!(),
    )
    .unwrap();
    let resources = resources.insert(loaded.resources);
    let mut options = Options::parse(std::iter::empty()).unwrap().unwrap();
    options.resources = root.to_path_buf();
    options.game_dir = Some(home.to_path_buf());
    options.player_name = Some("Host".into());
    options.execution_role = crate::game::execution_role::ExecutionRole::IntegratedServer;
    let mut game = Game::new(
        resources,
        loaded.pack,
        loaded.gui.unwrap(),
        options,
        settings,
        Audio::silent(&horizon_assets::resource_source::ResourceSource::directory(
            root.to_path_buf(),
        )),
        None,
    )
    .unwrap();
    game.build_registries();
    let rules = Arc::new(horizon_client::gamerules::RuleRegistry::vanilla());
    let create = horizon_client::title::WorldOptions {
        world_type: horizon_client::save::WorldPreset::Flat,
        rules: horizon_client::gamerules::GameRules::with_registry(rules.clone(), &[]),
        ..Default::default()
    };
    let mut request = crate::game::world_loading::WorldRequest::new(
        "regions",
        Some("regions"),
        Some(1),
        true,
        Some(&create),
        rules,
    )
    .unwrap();
    request.save_root = home.join("saves/regions");
    game.load_world(request).unwrap();
    game
}

#[test]
#[ignore = "daily"]
fn players_in_separate_regions_teleport_to_each_other_within_a_level_and_across_levels() {
    let root = std::path::PathBuf::from(std::env::var_os("HORIZON_TEST_RESOURCES").unwrap());
    let home =
        std::path::PathBuf::from(std::env::var_os("HORIZON_HOME").unwrap()).join("region-tp");
    let _ = std::fs::remove_dir_all(&home);
    let mut resources = None;
    let mut game = open_flat(&mut resources, &root, &home);
    // A dedicated server ticking by regions, on this thread.
    game.options.dedicated = Some(Vec::new());
    game.options.execution_role = crate::game::execution_role::ExecutionRole::DedicatedServer;
    horizon_client::parallel::set_setting(horizon_client::graphics::ParallelTickMode::On);
    crate::game::regions::tests::REGIONS_FORCED.with(|f| f.set(true));
    game.set_allow_commands(true);
    // The ticks run as fast as the machine does: no autosave in the middle of the test.
    game.ticks_until_autosave = i64::MAX / 2;
    let net = horizon_client::lan::NetConfig::loopback(0, "regions".into(), 8);
    let settings = crate::game::lan::LanSettings {
        port: 0,
        guest_commands: true,
        force_game_mode: false,
    };
    let hub = horizon_client::lan::hub::HubConfig {
        default_game_mode: 1,
        guest_commands: true,
        force_game_mode: false,
        hardcore: false,
        view_distance: 2,
        simulation_distance: 2,
        dimension: game.dim.id().to_owned(),
        allow_flight: true,
        seed: 1,
        max_players: 8,
        integrated: false,
        levels: game.level_shapes,
        level_seeds: game.levels.runtime.seeds(),
    };
    game.lan_host(settings, net, hub).unwrap();
    let port = game.lan.as_ref().unwrap().settings.port;
    assert_ne!(port, 0);

    // Three bots join at the world spawn.
    let mut bots = Vec::new();
    for name in ["Alpha", "Bravo", "Charlie"] {
        let joining = std::thread::spawn(move || Bot::connect(port, name));
        until(&mut game, "a bot's login", 120, |_| joining.is_finished());
        let bot = joining.join().unwrap().unwrap();
        BOTS.with(|b| {
            b.borrow_mut().push(Arc::new(BotSeen {
                name: bot.name,
                seen: Arc::clone(&bot.seen),
            }))
        });
        bots.push(bot);
    }
    let [alpha, bravo, charlie] = &bots[..] else {
        unreachable!()
    };
    until(&mut game, "the bots in the world", 120, |g| {
        bots.iter()
            .all(|b| find(g, b.name).is_some() && b.position().is_some())
    });
    let y = find(&mut game, "Alpha").unwrap().position[1];
    // They are operators.
    let hub = &mut game.lan.as_mut().unwrap().hub;
    for b in &bots {
        hub.ops.insert(offline_uuid(b.name), 4);
    }
    hub.refresh_permissions();

    // Bravo and Charlie are moved far from Alpha and from each other: three regions.
    let (bravo_at, charlie_at) = ([4800.5, y, 5.5], [-4800.5, y, 4800.5]);
    for (name, at) in [("Bravo", bravo_at), ("Charlie", charlie_at)] {
        let conn = game
            .lan
            .as_ref()
            .unwrap()
            .hub
            .guests
            .iter()
            .find(|g| g.name == name)
            .unwrap()
            .conn;
        game.teleport_to(Target::Guest(conn), at, None, true);
    }
    until(&mut game, "three regions of the Overworld", 240, |g| {
        regions_of(g, Dim::Overworld) == 3
            && bots
                .iter()
                .all(|b| find(g, b.name).is_some_and(|h| h.region.is_some()))
    });
    until(&mut game, "the bots told where they stand", 60, |_| {
        is(bravo.position(), bravo_at) && is(charlie.position(), charlie_at)
    });

    // Within a level: Alpha types `/tp Bravo`, and Bravo's region takes Alpha in.
    alpha.command("tp Bravo");
    until(&mut game, "Alpha at Bravo, in Bravo's region", 120, |g| {
        let (a, b) = (find(g, "Alpha"), find(g, "Bravo"));
        a.zip(b).is_some_and(|(a, b)| {
            is(Some(a.position), bravo_at) && a.region.is_some() && a.region == b.region
        }) && is(alpha.position(), bravo_at)
    });
    assert_eq!(alpha.seen.lock().unwrap().notfound, 0);
    assert!(is(find(&mut game, "Bravo").map(|h| h.position), bravo_at));
    // The other region went on by itself: Charlie never came home for it.
    assert!(is(charlie.position(), charlie_at));

    // Alpha goes to ground no region owns: a region of its own is made for it.
    alpha.command("tp Alpha 0.5 ~ 0.5");
    until(&mut game, "Alpha alone in a region of its own", 240, |g| {
        let (a, b) = (find(g, "Alpha"), find(g, "Bravo"));
        a.zip(b).is_some_and(|(a, b)| {
            is(Some(a.position), [0.5, y, 0.5]) && a.region.is_some() && a.region != b.region
        }) && regions_of(g, Dim::Overworld) == 3
    });
    assert!(is(alpha.position(), [0.5, y, 0.5]));

    // Into another level: Bravo is sent to the Nether and stands there alone (the review's last row, which
    // worked before: a command whose place is in another level).
    bravo.command("execute in minecraft:the_nether run tp @s 100.5 70 100.5");
    until(&mut game, "Bravo in the Nether", 300, |g| {
        find(g, "Bravo").is_some_and(|h| h.dimension == Dim::Nether.id())
            && bravo.dimension() == Dim::Nether.id()
    });

    // The review's failing row: out of a level, to a player in a region of another. Bravo types `/tp Charlie`;
    // Charlie's region answers with their place, and Bravo arrives in the Overworld at it.
    bravo.command("tp Charlie");
    until(
        &mut game,
        "Bravo at Charlie, in Charlie's region",
        300,
        |g| {
            let (b, c) = (find(g, "Bravo"), find(g, "Charlie"));
            b.zip(c).is_some_and(|(b, c)| {
                is(Some(b.position), charlie_at)
                    && b.dimension == Dim::Overworld.id()
                    && b.region.is_some()
                    && b.region == c.region
            }) && is(bravo.position(), charlie_at)
        },
    );
    assert_eq!(bravo.dimension(), Dim::Overworld.id());
    assert_eq!(
        bravo.seen.lock().unwrap().notfound,
        0,
        "no \"No entity was found\""
    );
    assert!(bravo.seen.lock().unwrap().left.is_none());
    // Into another level from a region: Charlie goes to the Nether, and Alpha, in a region of the Overworld,
    // follows with `/tp Charlie`.
    charlie.command("execute in minecraft:the_nether run tp @s 100.5 70 100.5");
    until(&mut game, "Charlie in the Nether", 300, |g| {
        find(g, "Charlie").is_some_and(|h| h.dimension == Dim::Nether.id())
            && charlie.dimension() == Dim::Nether.id()
    });
    alpha.command("tp Charlie");
    until(&mut game, "Alpha at Charlie in the Nether", 300, |g| {
        find(g, "Alpha").is_some_and(|h| {
            is(Some(h.position), [100.5, 70.0, 100.5]) && h.dimension == Dim::Nether.id()
        }) && is(alpha.position(), [100.5, 70.0, 100.5])
            && alpha.dimension() == Dim::Nether.id()
    });
    for b in &bots {
        assert!(
            b.seen.lock().unwrap().left.is_none(),
            "{} was kicked",
            b.name
        );
    }
}

/// A component's debug text with its strings spelled out (`JavaString([104, 105])` is `"hi"`).
fn readable(debug: &str) -> String {
    let mut out = String::new();
    let mut rest = debug;
    while let Some(i) = rest.find("JavaString([") {
        out.push_str(&rest[..i]);
        let after = &rest[i + "JavaString([".len()..];
        let end = after.find("])").unwrap_or(after.len());
        let text: String = after[..end]
            .split(", ")
            .filter_map(|n| n.parse::<u8>().ok())
            .map(char::from)
            .collect();
        out.push('"');
        out.push_str(&text);
        out.push('"');
        rest = &after[(end + 2).min(after.len())..];
    }
    out.push_str(rest);
    out
}

/// A dedicated server of a flat world ticking by regions on this thread, open on a loopback port; its port.
fn serve_by_regions<'a>(
    resources: &'a mut Option<Arc<Resources>>,
    folder: &str,
    view: i32,
) -> (Game<'a>, u16) {
    let root = std::path::PathBuf::from(std::env::var_os("HORIZON_TEST_RESOURCES").unwrap());
    let home = std::path::PathBuf::from(std::env::var_os("HORIZON_HOME").unwrap()).join(folder);
    let _ = std::fs::remove_dir_all(&home);
    let mut game = open_flat(resources, &root, &home);
    game.options.dedicated = Some(Vec::new());
    game.options.execution_role = crate::game::execution_role::ExecutionRole::DedicatedServer;
    horizon_client::parallel::set_setting(horizon_client::graphics::ParallelTickMode::On);
    crate::game::regions::tests::REGIONS_FORCED.with(|f| f.set(true));
    game.set_allow_commands(true);
    game.ticks_until_autosave = i64::MAX / 2;
    let net = horizon_client::lan::NetConfig::loopback(0, "regions".into(), 8);
    let settings = crate::game::lan::LanSettings {
        port: 0,
        guest_commands: true,
        force_game_mode: false,
    };
    let hub = horizon_client::lan::hub::HubConfig {
        default_game_mode: 1,
        guest_commands: true,
        force_game_mode: false,
        hardcore: false,
        view_distance: view,
        simulation_distance: view,
        dimension: game.dim.id().to_owned(),
        allow_flight: true,
        seed: 1,
        max_players: 8,
        integrated: false,
        levels: game.level_shapes,
        level_seeds: game.levels.runtime.seeds(),
    };
    game.lan_host(settings, net, hub).unwrap();
    let port = game.lan.as_ref().unwrap().settings.port;
    (game, port)
}

/// Bots that walk across new ground in separate regions: each region loads, lights and sends the chunks ahead of
/// its player itself. No landing is for terrain or for an area that changed, every chunk a bot's view holds is
/// lit in its region's world, and the bot was sent each of them.
#[test]
#[ignore = "daily"]
fn bots_exploring_in_separate_regions_load_their_terrain_without_landing() {
    let mut resources = None;
    let (mut game, port) = serve_by_regions(&mut resources, "region-explore", 3);
    let mut bots = Vec::new();
    for name in ["Alpha", "Bravo"] {
        let joining = std::thread::spawn(move || Bot::connect(port, name));
        until(&mut game, "a bot's login", 120, |_| joining.is_finished());
        let bot = joining.join().unwrap().unwrap();
        BOTS.with(|b| {
            b.borrow_mut().push(Arc::new(BotSeen {
                name: bot.name,
                seen: Arc::clone(&bot.seen),
            }))
        });
        bots.push(bot);
    }
    until(&mut game, "the bots in the world", 120, |g| {
        bots.iter()
            .all(|b| find(g, b.name).is_some() && b.position().is_some())
    });
    let y = find(&mut game, "Alpha").unwrap().position[1];
    let conn = |game: &Game<'_>, name: &str| {
        game.lan
            .as_ref()
            .unwrap()
            .hub
            .guests
            .iter()
            .find(|g| g.name == name)
            .map(|g| g.conn)
    };
    // Bravo far from Alpha: two regions.
    let start = [[0.5, 0.5], [4800.5, 5.5]];
    let bravo_conn = conn(&game, "Bravo").unwrap();
    game.teleport_to(Target::Guest(bravo_conn), [4800.5, y, 5.5], None, true);
    until(&mut game, "two regions of the Overworld", 240, |g| {
        regions_of(g, Dim::Overworld) == 2
            && bots
                .iter()
                .all(|b| find(g, b.name).is_some_and(|h| h.region.is_some()))
    });
    until(&mut game, "Bravo told where it stands", 60, |_| {
        is(bots[1].position(), [4800.5, y, 5.5])
    });
    // Both walk on, a flying player's pace: a block every other tick, 600 blocks (nearly forty sections of new
    // ground each).
    let mut tick = 0u32;
    until(&mut game, "the walk", 240, |_| {
        tick += 1;
        if tick % 2 == 0 {
            let along = f64::from(tick / 2);
            bots[0].step_to(start[0][0] + along, y, start[0][1]);
            bots[1].step_to(start[1][0], y, start[1][1] + along);
        }
        tick >= 1200
    });
    // Let the last chunks arrive.
    let mut still = 0u32;
    until(&mut game, "the terrain around the bots", 120, |g| {
        still += 1;
        still > 100
            && bots.iter().all(|b| {
                let h = find(g, b.name).unwrap();
                let (cx, cz) = (
                    (h.position[0].floor() as i32) >> 4,
                    (h.position[2].floor() as i32) >> 4,
                );
                let seen = b.seen.lock().unwrap();
                (cx - 3..=cx + 3).all(|x| (cz - 3..=cz + 3).all(|z| seen.chunks.contains(&(x, z))))
            })
    });
    // A third player joins at the world spawn, where no region is any more: a region is made there, and its
    // streamer loads the spawn's chunks.
    let before = regions_of(&game, Dim::Overworld);
    let joining = std::thread::spawn(move || Bot::connect(port, "Charlie"));
    until(&mut game, "a late bot's login", 120, |_| joining.is_finished());
    let charlie = joining.join().unwrap().unwrap();
    until(&mut game, "Charlie in a region of their own", 240, |g| {
        regions_of(g, Dim::Overworld) == before + 1
            && find(g, "Charlie").is_some_and(|h| h.region.is_some())
            && (-3..=3).all(|x| {
                (-3..=3).all(|z| charlie.seen.lock().unwrap().chunks.contains(&(x, z)))
            })
    });
    assert!(regions_of(&game, Dim::Overworld) >= 2, "the regions stayed out");
    // They did walk: the server holds each where the walk ended, hundreds of blocks on.
    assert!(find(&mut game, "Alpha").unwrap().position[0] > 500.0);
    assert!(find(&mut game, "Bravo").unwrap().position[2] > 500.0);
    for why in ["terrain", "area"] {
        assert_eq!(
            game.regions.prof.landed_for(why),
            0,
            "no landing for {why}: {:?}",
            game.regions.prof.landed_for("region wants")
        );
    }
    // Every chunk in a bot's view is in its region's world and lit.
    for b in &bots {
        let h = find(&mut game, b.name).unwrap();
        let (cx, cz) = (
            (h.position[0].floor() as i32) >> 4,
            (h.position[2].floor() as i32) >> 4,
        );
        let slot = game
            .regions
            .resident
            .values_mut()
            .flat_map(|s| s.games.iter_mut())
            .find(|s| Some(s.uid) == h.region)
            .unwrap();
        let w = slot.game.as_deref().unwrap();
        for x in cx - 3..=cx + 3 {
            for z in cz - 3..=cz + 3 {
                let c = w.world.chunks.get(&(x, z));
                assert!(
                    c.is_some_and(|c| c.lit),
                    "{}: chunk {x},{z} is missing or dark in region {:?}",
                    b.name,
                    h.region
                );
            }
        }
        assert!(b.seen.lock().unwrap().left.is_none(), "{} was kicked", b.name);
    }
}

/// Every chunk of `world` with its eight neighbours is lit as its blocks give: its light is what lighting it again
/// from scratch gives. How many chunks were compared.
fn light_matches_blocks(
    world: &horizon_client::world::World,
    tables: &horizon_client::light::LightTables,
) -> std::result::Result<usize, String> {
    use horizon_client::light::{apply_lighting, light_chunk};
    let mut compared = 0;
    let mut wrong: Vec<String> = Vec::new();
    for (&(x, z), chunk) in world.chunks.iter() {
        let around: [[Option<&horizon_client::chunk::Chunk>; 3]; 3] = std::array::from_fn(|dz| {
            std::array::from_fn(|dx| {
                world
                    .chunks
                    .get(&(x + dx as i32 - 1, z + dz as i32 - 1))
                    .map(|c| &**c)
            })
        });
        if !chunk.lit || around.iter().flatten().any(Option::is_none) {
            continue;
        }
        let mut again = (**chunk).clone();
        apply_lighting(&mut again, light_chunk(&around, tables, true));
        let (mut count, mut first) = (0, None);
        for sy in 0..world.layout().sections() as i32 * 16 {
            let y = world.layout().min_y() + sy;
            for bz in 0..16 {
                for bx in 0..16 {
                    let (stored, scratch) = (chunk.light(bx, y, bz), again.light(bx, y, bz));
                    if stored != scratch {
                        count += 1;
                        first.get_or_insert((bx, y, bz, stored, scratch));
                    }
                }
            }
        }
        if let Some((bx, y, bz, stored, scratch)) = first {
            wrong.push(format!(
                "chunk {x},{z}: {count} cells differ, first {bx},{y},{bz}: stored {stored}, from its blocks {scratch} (lit {})",
                chunk.lit
            ));
        }
        compared += 1;
    }
    if wrong.is_empty() {
        Ok(compared)
    } else {
        wrong.sort();
        Err(format!("{} of {compared} chunks:\n{}", wrong.len(), wrong.join("\n")))
    }
}

/// A big `/fill` typed by a player, up to the edge of the chunks loaded: the light of every chunk is the light its
/// blocks give. `regions`: the player stands in a region (with another far away); without, the level is whole.
fn a_big_fill_converges(regions: bool) {
    let mut resources = None;
    let (mut game, port) = serve_by_regions(&mut resources, "region-fill", 3);
    let names: &[&'static str] = if regions {
        &["Alpha", "Bravo"]
    } else {
        &["Alpha"]
    };
    let mut bots = Vec::new();
    for &name in names {
        let joining = std::thread::spawn(move || Bot::connect(port, name));
        until(&mut game, "a bot's login", 120, |_| joining.is_finished());
        let bot = joining.join().unwrap().unwrap();
        BOTS.with(|b| {
            b.borrow_mut().push(Arc::new(BotSeen {
                name: bot.name,
                seen: Arc::clone(&bot.seen),
            }))
        });
        bots.push(bot);
    }
    until(&mut game, "the bots in the world", 120, |g| {
        bots.iter()
            .all(|b| find(g, b.name).is_some() && b.position().is_some())
    });
    let y = find(&mut game, "Alpha").unwrap().position[1];
    let hub = &mut game.lan.as_mut().unwrap().hub;
    for b in &bots {
        hub.ops.insert(offline_uuid(b.name), 4);
    }
    hub.refresh_permissions();
    if regions {
        let bravo_conn = game
            .lan
            .as_ref()
            .unwrap()
            .hub
            .guests
            .iter()
            .find(|g| g.name == "Bravo")
            .unwrap()
            .conn;
        game.teleport_to(Target::Guest(bravo_conn), [4800.5, y, 5.5], None, true);
        until(&mut game, "two regions", 240, |g| {
            regions_of(g, Dim::Overworld) == 2
                && bots
                    .iter()
                    .all(|b| find(g, b.name).is_some_and(|h| h.region.is_some()))
        });
    }
    let mut waited = 0;
    until(&mut game, "the terrain around Alpha", 120, |_| {
        waited += 1;
        waited > 100
            && (-3..=3).all(|x| {
                (-3..=3).all(|z| bots[0].seen.lock().unwrap().chunks.contains(&(x, z)))
            })
    });
    // Alpha lights the sky and the ground over 31 x 21 x 31 blocks from two chunks east to the third, then
    // hollows it out.
    bots[0].command("fill ~20 ~ ~-15 ~50 ~20 ~15 minecraft:glowstone");
    bots[0].command("fill ~25 ~5 ~-5 ~45 ~15 ~5 minecraft:air");
    let mut ticks = 0;
    until(&mut game, "the fill made and its light settled", 240, |_| {
        ticks += 1;
        ticks > 400
    });
    let tables = game.pack.context.light.clone();
    let compared = if regions {
        let h = find(&mut game, "Alpha").unwrap();
        let slot = game
            .regions
            .resident
            .values_mut()
            .flat_map(|s| s.games.iter_mut())
            .find(|s| Some(s.uid) == h.region)
            .unwrap();
        let w = slot.game.as_deref().unwrap();
        assert!(w.world.light_edits.is_empty() && w.world.dirty_light.is_empty());
        light_matches_blocks(&w.world, &tables)
    } else {
        light_matches_blocks(&game.world, &tables)
    };
    let compared = compared.unwrap_or_else(|e| panic!("regions {regions}: {e}"));
    assert!(compared > 20, "chunks compared: {compared}");
}

#[test]
#[ignore = "daily"]
fn a_big_fill_in_a_region_converges_to_the_light_of_its_blocks() {
    a_big_fill_converges(true);
}

/// The same fill with regions off: the light the level itself leaves, what the region's must match.
#[test]
#[ignore = "daily"]
fn a_big_fill_in_a_whole_level_converges_to_the_light_of_its_blocks() {
    a_big_fill_converges(false);
}
