//! A guest's `horizon:perf` requests (docs/perf-platform.md §4.2): who may profile this server, the one capture at a time, and the log lines about it.
use super::Game;
use super::integrated::capture::{Live, Target};
use super::profile_capture::{reports_dir, shown};
use horizon_client::perf_net::{
    CHANNEL, Held, OPERATOR_LEVEL, Outgoing, Refusal, Reply, Request, Verdict, Wanted, judge,
};
use horizon_perf_format::v1::{StopReason, Trigger};
use horizon_protocol::packets::{configuration::CustomPayload, play};
use std::fs::File;
use std::io::Read;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::time::Instant;

/// The guest whose capture runs on this server.
pub(in crate::game) struct Requester {
    conn: u32,
    name: String,
    /// Their connection ended: the capture stops and its file stays on the server.
    left: bool,
}

/// A request that passed the checks and waits for the tick loop that owns the capture state.
pub(in crate::game) enum Asked {
    Start {
        conn: u32,
        name: String,
        wanted: Wanted,
    },
    Stop(StopReason),
}

/// What a guest has asked of this server, for the tick loop.
#[derive(Default)]
pub(in crate::game) struct Served {
    requester: Option<Requester>,
    asked: Vec<Asked>,
    sending: Vec<Sending>,
}

/// A finished capture's file on its way to the guest that asked for it.
struct Sending {
    conn: u32,
    file: File,
    pace: Outgoing,
}

/// How a server capture's file ended.
pub(in crate::game) enum Done {
    Saved {
        path: String,
        file: PathBuf,
        removed: Vec<String>,
    },
    Failed,
}

fn payload(reply: &Reply) -> play::Clientbound {
    play::Clientbound::CustomPayload(CustomPayload {
        channel: CHANNEL.into(),
        data: reply.encode(),
    })
}

fn window(wanted: &Wanted) -> String {
    wanted
        .seconds
        .map_or_else(|| "until stopped".to_owned(), |s| format!("{s} s"))
}

impl Game<'_> {
    /// A `horizon:perf` payload from `conn`.
    pub(super) fn guest_perf(&mut self, conn: u32, data: &[u8]) {
        let Some(guest) = self.lan.as_ref().and_then(|host| host.hub.guest(conn)) else {
            return;
        };
        let name = guest.name.clone();
        let Ok(request) = Request::decode(data) else {
            log::debug!("Horizon: a bad server profile request from {name}");
            return;
        };
        let served = &mut self.profile.served;
        let held = match &served.requester {
            Some(r) if r.conn == conn => Held::Asker,
            Some(_) => Held::Someone,
            None if served
                .asked
                .iter()
                .any(|a| matches!(a, Asked::Start { .. })) =>
            {
                Held::Someone
            }
            None => Held::Nothing,
        };
        match judge(&guest.brand, guest.permission, &request, held) {
            Verdict::Ignore => {}
            Verdict::Refuse(why) => {
                if why == Refusal::Denied {
                    log::warn!(
                        "Horizon: {name} asked for a server profile and was refused: operator level {OPERATOR_LEVEL} is required"
                    );
                }
                guest.send(&payload(&Reply::Refused(why)));
            }
            Verdict::Accept => served.asked.push(match request {
                Request::Start(wanted) => Asked::Start { conn, name, wanted },
                Request::Stop => Asked::Stop(StopReason::Normal),
            }),
        }
    }

    /// Whether the player on `conn` is online: in the server's own hub, or away in a resident region, which
    /// holds them between its ticks. A region's lend is not a leave.
    fn perf_player_online(&self, conn: u32) -> bool {
        self.lan
            .as_ref()
            .is_some_and(|h| h.hub.guest(conn).is_some() || h.hub.away.contains(&conn))
    }

    /// Sends `reply` to the player on `conn`, if they are still online: straight away when they are here, else
    /// to their region, which delivers it before its next tick.
    pub(in crate::game) fn perf_reply(&mut self, conn: u32, reply: &Reply) {
        if let Some(guest) = self.lan.as_ref().and_then(|h| h.hub.guest(conn)) {
            guest.send(&payload(reply));
            return;
        }
        let away = self
            .away_server_players()
            .find(|p| p.conn == conn)
            .map(|p| p.uuid);
        if let Some(uuid) = away {
            self.queue_for_away(uuid, payload(reply), true);
        }
    }

    /// The bytes sent to the player on `conn` and not framed yet, wherever they are; None once they are gone.
    fn perf_backlog(&self, conn: u32) -> Option<usize> {
        if let Some(guest) = self.lan.as_ref().and_then(|h| h.hub.guest(conn)) {
            return Some(guest.sender.pending.load(Ordering::Relaxed));
        }
        self.away_server_players()
            .find(|p| p.conn == conn)
            .map(|p| p.sender.pending.load(Ordering::Relaxed))
    }

    /// What guests asked since the last tick, and a stop when the guest whose capture runs has left.
    pub(in crate::game) fn take_perf_asked(&mut self) -> Vec<Asked> {
        let mut asked = std::mem::take(&mut self.profile.served.asked);
        let gone = self
            .profile
            .served
            .requester
            .as_ref()
            .is_some_and(|r| !r.left && !self.perf_player_online(r.conn));
        if gone && let Some(r) = self.profile.served.requester.as_mut() {
            r.left = true;
            log::info!("Horizon: {} left; their server profile stops", r.name);
            asked.push(Asked::Stop(StopReason::Disconnected));
        }
        asked
    }

    /// The capture `name` asked for runs, in the file at `path`.
    pub(in crate::game) fn perf_started(
        &mut self,
        conn: u32,
        name: String,
        wanted: &Wanted,
        path: &str,
    ) {
        log::info!(
            "Horizon: {name} started a server profile ({}, samples {})",
            window(wanted),
            if wanted.samples { "on" } else { "off" }
        );
        self.profile.served.requester = Some(Requester {
            conn,
            name,
            left: false,
        });
        self.perf_reply(
            conn,
            &Reply::Started {
                path: path.to_owned(),
                seconds: wanted.seconds,
                samples: wanted.samples,
            },
        );
    }

    /// The capture `conn` asked for could not begin.
    pub(in crate::game) fn perf_refused(&mut self, conn: u32, why: Refusal) {
        self.perf_reply(conn, &Reply::Refused(why));
    }

    /// Server captures whose files are done: the guest that asked is told, and the log names it.
    pub(in crate::game) fn perf_captures_done(&mut self, done: Vec<Done>) {
        for done in done {
            let Some(Requester { conn, name, left }) = self.profile.served.requester.take() else {
                continue;
            };
            if left {
                if let Done::Saved { path, .. } = &done {
                    log::info!("Horizon: {name}'s server profile saved to {path}");
                }
                continue;
            }
            match done {
                Done::Saved {
                    path,
                    file,
                    removed,
                } => {
                    log::info!("Horizon: {name}'s server profile saved to {path}");
                    let (bytes, sending) = match File::open(&file).and_then(|f| {
                        let len = f.metadata()?.len();
                        Ok((f, len))
                    }) {
                        Ok((file, len)) => (
                            len,
                            Some(Sending {
                                conn,
                                file,
                                pace: Outgoing::new(len, Instant::now()),
                            }),
                        ),
                        Err(e) => {
                            log::warn!("Horizon: {name}'s server profile cannot be sent: {e}");
                            (0, None)
                        }
                    };
                    self.perf_reply(
                        conn,
                        &Reply::Finished {
                            path,
                            removed,
                            bytes,
                        },
                    );
                    self.profile.served.sending.extend(sending);
                }
                Done::Failed => {
                    log::error!("Horizon: {name}'s server profile could not be written");
                    self.perf_refused(conn, Refusal::Failed);
                }
            }
        }
    }

    /// Sends the next chunk of each file that is due, at most [`horizon_client::perf_net::RATE`] a second and not while the guest has a backlog.
    pub(in crate::game) fn pump_perf_files(&mut self) {
        let now = Instant::now();
        let mut sending = std::mem::take(&mut self.profile.served.sending);
        let mut out = Vec::new();
        sending.retain_mut(|s| {
            let Some(queued) = self.perf_backlog(s.conn) else {
                return false;
            };
            let Some(n) = s.pace.next(now, queued) else {
                return !s.pace.done();
            };
            let mut chunk = vec![0; n];
            match s.file.read_exact(&mut chunk) {
                Ok(()) => out.push((s.conn, Reply::Chunk(chunk))),
                Err(e) => {
                    log::warn!("Horizon: a server profile could not be read for sending: {e}");
                    out.push((s.conn, Reply::Refused(Refusal::Failed)));
                    return false;
                }
            }
            !s.pace.done()
        });
        for (conn, reply) in out {
            self.perf_reply(conn, &reply);
        }
        sending.append(&mut self.profile.served.sending);
        self.profile.served.sending = sending;
    }

    /// On the server thread, between two ticks: begins and ends the captures guests asked for.
    pub(super) fn serve_perf_asked(&mut self) {
        self.pump_perf_files();
        for asked in self.take_perf_asked() {
            match asked {
                Asked::Start { conn, name, wanted } => {
                    if self.server_capture_busy() {
                        self.perf_refused(conn, Refusal::AlreadyRunning);
                        continue;
                    }
                    let target = Target::Own {
                        options: wanted.options(),
                        trigger: Trigger::Remote,
                        dir: reports_dir(),
                    };
                    match self.start_server_capture(target, Live::new()) {
                        Ok(path) => self.perf_started(conn, name, &wanted, &shown(&path)),
                        Err(e) => {
                            log::error!("Horizon: {name}'s server profile did not start: {e}");
                            self.perf_refused(conn, Refusal::Failed);
                        }
                    }
                }
                Asked::Stop(reason) => self.stop_server_capture(reason, None),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn asker_of(game: &mut Game<'_>, conn: u32) {
        game.profile.served.requester = Some(Requester {
            conn,
            name: "guest12".into(),
            left: false,
        });
    }

    #[test]
    #[ignore = "daily"]
    fn a_profile_stays_with_its_player_while_their_region_has_them_and_stops_when_they_leave() {
        crate::game::native_guest_turn_tests::with_guests(|game| {
            asker_of(game, 12);
            assert!(game.take_perf_asked().is_empty(), "the player is here");
            // Their region is lent them: they are away, not gone.
            let host = game.lan.as_mut().unwrap();
            let lent = host
                .hub
                .split_guests(1, |conn| (conn == 12).then_some(0))
                .remove(0);
            host.hub.away.insert(12);
            assert!(host.hub.guest(12).is_none());
            for _ in 0..3 {
                assert!(game.take_perf_asked().is_empty(), "a lend is not a leave");
            }
            assert!(!game.profile.served.requester.as_ref().unwrap().left);
            // They leave from their region.
            drop(lent);
            game.lan.as_mut().unwrap().hub.away.remove(&12);
            let asked = game.take_perf_asked();
            assert!(matches!(asked[..], [Asked::Stop(StopReason::Disconnected)]));
        });
    }
}
