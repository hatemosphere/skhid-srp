//! `SRP_DEV_REPLAY` (`game/playback.rs`): a spectator's game shown a
//! recording instead of the match it asked to watch. The recording is a
//! real one: a two-place Team Deathmatch on level_01 of 2026-11-07, 114.6 s,
//! made into a `.sur` by `srp-recording-to-sur` (with an empty mode config).
//! Its key frames: 50, 14 500, 24 500, 39 550, 49 550, 59 550, 69 550,
//! 81 500, 94 550, 104 600 and 114 600 ms.
//! Its worlds, inputs and roster changes are played as they are; its 0xE4
//! is the tests' (its profiles hold items of the live game data, which the
//! tests' dictionary does not have).

use std::path::PathBuf;
use std::sync::atomic::AtomicU32;

use survarium::v026g::maps::MatchMap;
use survarium::v026g::match_msgs::static_info::{MatchOptions, SCORE_WEIGHTS, StaticInfo};
use survarium::v026g::match_msgs::{PlayerInput, ServerMessage, TICK_MS, WireTime};

use super::spectate::{SPECTATOR, SPECTATOR_ACCOUNT, watch};
use super::*;
use crate::v026g::game::{Control, Speed};
use crate::v026g::sur;

const RECORDING: &str = "tests/data/replay_level_01_tdm_2p.sur";
fn recording() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(RECORDING)
}

/// The recording with the tests' 0xE4 (two places of level_01's Team
/// Deathmatch), in a file of its own.
fn playable() -> PathBuf {
    let options = MatchOptions {
        players: vec![profile("a", 0), profile("b", 1)],
        map: MatchMap::of(MAP_TDM).unwrap(),
        match_id: 0x1234,
        match_level: 3,
        score_weights: SCORE_WEIGHTS,
    };
    let info = StaticInfo::new(&options, Dictionary::shared()).unwrap();
    let options = info.payload(0).unwrap()[1..].to_vec();
    let file = std::fs::read(recording()).unwrap();
    let mut out = Vec::new();
    for (id, bytes) in sur::chunks(&file).unwrap() {
        let bytes = if id == sur::chunk::OPTIONS {
            &options
        } else {
            bytes
        };
        out.extend(id.to_le_bytes());
        out.extend((bytes.len() as u32).to_le_bytes());
        out.extend(bytes);
    }
    static FILES: AtomicU32 = AtomicU32::new(0);
    let path = std::env::temp_dir().join(format!(
        "srp-playback-{}-{}.sur",
        std::process::id(),
        FILES.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&path, out).unwrap();
    path
}

/// The recording's inputs of `player`, in order: time and input.
fn recorded(player: u8) -> Vec<(u32, PlayerInput)> {
    let file = std::fs::read(recording()).unwrap();
    let contents = sur::read(&file).unwrap();
    contents
        .records
        .iter()
        .flat_map(|record| {
            let time = record.event.time;
            record
                .event
                .inputs
                .iter()
                .filter(move |(p, _)| *p == player)
                .map(move |(_, value)| {
                    let input = PlayerInput {
                        time: if time % TICK_MS == 0 {
                            WireTime::tick(time)
                        } else {
                            WireTime::precise(time)
                        },
                        rotation: value.rotation,
                        actions: value.actions,
                    };
                    (time, input)
                })
        })
        .collect()
}

/// As `Client::relays_of` reads them: each distinct input once, the first.
fn distinct(inputs: impl IntoIterator<Item = PlayerInput>) -> Vec<PlayerInput> {
    let mut out = Vec::new();
    for input in inputs {
        if !out.contains(&input) {
            out.push(input);
        }
    }
    out
}

/// A bench whose server shows the recording to every spectator; nobody
/// plays its match.
fn bench() -> Bench {
    let settings = Settings {
        dev_replay: Some(playable()),
        ..Settings::default()
    };
    Bench::growing(2, MAP_BATTERIES, settings)
}

/// A spectator's game connected and loaded, with its first world: its
/// client's index.
fn watching(bench: &mut Bench) -> usize {
    watch(bench, SPECTATOR);
    let at = bench.clients.len();
    bench.clients.push(Client::idle_growing(SPECTATOR));
    bench
        .addrs
        .push(SocketAddr::from(([127, 0, 0, 1], 45_000 + at as u16)));
    bench.clients[at].connect();
    assert!(bench.until(1_000, |b| b.clients[at].model.options.is_some()));
    assert_eq!(bench.clients[at].model.local_index, None);
    bench.clients[at].ready();
    assert!(
        bench.until(5_000, |b| b.clients[at].model.is_synced()),
        "{:?}",
        bench.clients[at].faults
    );
    at
}

fn position(bench: &Bench) -> u32 {
    bench
        .server
        .dev_replay
        .as_ref()
        .and_then(|replay| replay.position(SPECTATOR, bench.now()))
        .expect("a playback")
}

fn control(bench: &mut Bench, control: Control) {
    let now = bench.now();
    bench
        .server
        .dev_replay
        .as_mut()
        .unwrap()
        .control(control, now);
}

fn relays_taken(client: &Client) -> usize {
    client
        .got(|message| matches!(message, ServerMessage::InputRelay { .. }).then_some(()))
        .len()
}

#[test]
fn a_spectator_gets_the_first_world_then_the_relays_in_recorded_order() {
    let mut bench = bench();
    let at = watching(&mut bench);
    let spectator = &bench.clients[at];
    assert_eq!(spectator.snapshots(), [(0, 50)]);
    assert_eq!(spectator.model.options.as_ref().unwrap().players.len(), 2);
    // Player 0's first input is at 51.85 s.
    bench.run_long(60_000);
    let now = position(&bench);
    assert!((59_000..=61_000).contains(&now), "{now}");
    let spectator = &bench.clients[at];
    // The world came before any relay.
    let first = |pick: fn(&ServerMessage) -> bool| spectator.taken.iter().position(pick);
    assert!(
        first(|m| matches!(m, ServerMessage::Snapshot { .. }))
            < first(|m| matches!(m, ServerMessage::InputRelay { .. }))
    );
    for player in 0..2 {
        let all = recorded(player);
        let due = distinct(all.iter().filter(|(t, _)| *t <= now).map(|(_, i)| *i));
        let want = distinct(all.iter().map(|(_, input)| *input));
        let got = spectator.relays_of(player);
        assert!(!due.is_empty());
        assert!(
            got.len() >= due.len(),
            "player {player}: {} of {}",
            got.len(),
            due.len()
        );
        assert_eq!(got[..], want[..got.len()], "player {player}");
    }
    assert_eq!(spectator.entered(), [1, 0]);
    // Its world was never started again.
    assert_eq!(spectator.snapshots().len(), 1);
    bench.assert_no_faults();
}

#[test]
fn pause_stops_the_flow_and_play_starts_the_game_again_where_it_stopped() {
    let mut bench = bench();
    let at = watching(&mut bench);
    bench.run_long(20_000);
    control(&mut bench, Control::Pause);
    let paused = position(&bench);
    bench.run(500);
    let taken = relays_taken(&bench.clients[at]);
    bench.run_long(5_000);
    assert_eq!(position(&bench), paused);
    assert_eq!(relays_taken(&bench.clients[at]), taken);
    // The recording has inputs there: the pause held them.
    assert!(
        recorded(0)
            .iter()
            .chain(&recorded(1))
            .any(|(t, _)| (paused + 500..paused + 5_000).contains(t))
    );
    // Its game's clock ran on meanwhile: play starts it again from the key
    // frame before the pause, and the relays go on.
    let worlds = bench.clients[at].snapshots().len();
    control(&mut bench, Control::Play);
    assert!(bench.until(2_000, |b| b.clients[at].snapshots().len() > worlds));
    let (generation, time) = *bench.clients[at].snapshots().last().unwrap();
    assert_eq!(generation, 1);
    assert_eq!(time, 14_500);
    bench.run_long(3_000);
    assert!(relays_taken(&bench.clients[at]) > taken);
    bench.assert_no_faults();
}

#[test]
fn speed_scales_the_playback_clock_and_the_relays_keep_up() {
    let mut bench = bench();
    let at = watching(&mut bench);
    bench.run_long(5_000);
    for (speed, factor) in [
        (Speed::Double, 2.0),
        (Speed::Quadruple, 4.0),
        (Speed::Half, 0.5),
    ] {
        control(&mut bench, Control::Speed(speed));
        let from = position(&bench);
        bench.run_long(4_000);
        let went = f64::from(position(&bench) - from);
        assert!(
            (went / 4_000.0 - factor).abs() < 0.05,
            "{speed:?}: {went} ms"
        );
    }
    // Every input due by the clock reached it, at 4x too.
    let now = position(&bench);
    for player in 0..2 {
        let due = distinct(
            recorded(player)
                .into_iter()
                .filter(|(t, _)| *t <= now)
                .map(|(_, input)| input),
        );
        let got = bench.clients[at].relays_of(player);
        assert!(
            due.iter().all(|input| got.contains(input)),
            "player {player}"
        );
    }
    bench.assert_no_faults();
}

#[test]
fn seek_starts_the_game_again_from_the_key_frame_at_or_before_the_time() {
    let mut bench = bench();
    let at = watching(&mut bench);
    bench.run_long(3_000);
    // To 65 s a player's inputs since the key frame stop it short; to 20 s
    // the 5 s a tick byte reaches back.
    for (target, key_frame, resumed_at) in [
        (65_000, 59_550, 61_699),
        (20_000, 14_500, 19_500),
        (94_550, 94_550, 94_550),
    ] {
        let set_times = bench.clients[at].got(|m| match m {
            ServerMessage::SetTime { .. } => Some(()),
            _ => None,
        });
        let worlds = bench.clients[at].snapshots().len();
        control(&mut bench, Control::Seek(target));
        let resumed = position(&bench);
        assert_eq!(resumed, resumed_at);
        assert!(bench.until(2_000, |b| b.clients[at].snapshots().len() > worlds));
        let (_, time) = *bench.clients[at].snapshots().last().unwrap();
        assert_eq!(time, key_frame);
        // Its clock is put at the playback's (0xE5 of the new generation).
        let clocks = bench.clients[at].got(|m| match m {
            ServerMessage::SetTime { match_time, .. } => Some(*match_time),
            _ => None,
        });
        assert!(clocks[set_times.len()..].iter().all(|&t| t >= resumed));
        // The inputs since the key frame came with it, and go on after.
        bench.run_long(3_000);
        let now = position(&bench);
        for player in 0..2 {
            let got = bench.clients[at].relays_of(player);
            let missing: Vec<_> = recorded(player)
                .into_iter()
                .filter(|(t, input)| (key_frame + 1..=now).contains(t) && !got.contains(input))
                .collect();
            assert!(missing.is_empty(), "player {player}: {missing:?}");
        }
    }
    bench.assert_no_faults();
}

#[test]
fn the_end_of_the_recording_ends_the_watch() {
    let mut bench = bench();
    let at = watching(&mut bench);
    control(&mut bench, Control::Seek(112_000));
    assert!(
        bench.until(5_000, |b| b.clients[at].told_to_leave),
        "{}",
        position(&bench)
    );
    // Out of its queue: it has seen the match.
    assert_eq!(
        bench
            .server
            .store()
            .matchmaking()
            .take_watchers_left()
            .into_iter()
            .map(|(_, account)| account)
            .collect::<Vec<_>>(),
        [SPECTATOR_ACCOUNT]
    );
    // Then silence, longer than the 5 s that end a game's match link; then
    // it is forgotten.
    bench.run(300);
    let heard = bench.clients[at].datagrams_received;
    bench.run_long(6_000);
    assert_eq!(bench.clients[at].datagrams_received, heard);
    bench.run_long(1_000);
    assert!(
        bench
            .server
            .dev_replay
            .as_ref()
            .unwrap()
            .position(SPECTATOR, bench.now())
            .is_none()
    );
    bench.assert_no_faults();
}

#[test]
fn controls_read_as_written() {
    assert_eq!(Control::parse("pause"), Some(Control::Pause));
    assert_eq!(Control::parse(" play "), Some(Control::Play));
    assert_eq!(
        Control::parse("speed 0.5"),
        Some(Control::Speed(Speed::Half))
    );
    assert_eq!(Control::parse("seek 65.5"), Some(Control::Seek(65_500)));
    for wrong in ["speed 3", "seek", "seek -1", "pause now", "stop"] {
        assert_eq!(Control::parse(wrong), None, "{wrong}");
    }
}
