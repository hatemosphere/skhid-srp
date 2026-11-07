//! Dev only (`SRP_DEV_REPLAY`): a recorded match played to a spectator's
//! game, in place of the match it asked to watch, through the spectators'
//! own machine ([`spectate`]): the "replay match" of path A in
//! `research/replay-plan.md`. A spike; what a real client makes of pause,
//! speed and seek is not known yet (`docs/notes/replay-stream.md`).
//!
//! The recording is the `.sur` the converter makes (`sur_convert`; a
//! `.srprec` is converted on load): its 0xE4 (every place of the match, as
//! the replay viewer has them), its key frames as the worlds a spectator may
//! start from, and its inputs, kit choices, entries and leaves as the relays
//! and roster changes, at their recorded times.
//!
//! A playback clock says how far the match has gone. What is due by it
//! ([`RELAY_LEAD_MS`] ahead, as a live game gets an input a little before
//! its tick) goes to the spectator as from a live match. Pause stops the
//! clock; speed scales it; a seek, and play after a pause, start the game
//! again (0xE5 of its next generation, the world) from the key frame at or
//! before the time, put as far past it as the game's input lists reach
//! ([`Recording::reach`]). The end of the recording lets the game go (0xEA
//! naming no place), as the end of a live match does.

use std::path::Path;

use survarium::v026g::match_msgs::static_info::{self, NO_LOCAL_PLACE, StaticInfo};
use survarium::v026g::match_msgs::{PlayerInput, WireTime, mask_bytes};
use udp_transport::table::{Direction, Table};
use udp_transport::wire;

use super::spectate::{Heard, Spectator, Watched};
use super::*;
use crate::v026g::record;
use crate::v026g::sur::{self, record_type};
use crate::v026g::sur_convert;

/// What is due this long past the playback clock goes already: a live game
/// gets a player's input about this much before its tick (the server's
/// event horizon, [`HORIZON_MS`]).
const RELAY_LEAD_MS: u32 = HORIZON_MS;
/// How often the control file is read.
const CONTROL_EVERY: Duration = Duration::from_millis(250);

/// A recording made into what a spectator is shown.
pub(crate) struct Recording {
    info: StaticInfo,
    /// The key frames, in time order.
    worlds: Vec<Snapshot>,
    /// In time order.
    inputs: Vec<(u32, usize, Relayed)>,
    /// In time order.
    events: Vec<(u32, usize, bool)>,
    /// The time of its last record.
    end: u32,
}

impl Recording {
    /// A `.sur`, or a `.srprec` converted here (with no mode config: the
    /// `.sur`'s chunk 4 is the viewer's alone).
    pub(crate) fn load(path: &Path) -> Result<Self, String> {
        let read = |error: std::io::Error| format!("{}: {error}", path.display());
        let file = if path.extension().is_some_and(|ext| ext == "srprec") {
            let notes = record::read_notes(path).map_err(read)?;
            let settings = sur_convert::Settings {
                // As many key frames as a file may hold: more places to seek to.
                key_frame_every_ms: 1_000,
                ..sur_convert::Settings::default()
            };
            sur_convert::convert_file(path, &notes, &[], settings)
                .map_err(|error| error.to_string())?
                .0
        } else {
            std::fs::read(path).map_err(read)?
        };
        Self::from_sur(&file)
    }

    pub(crate) fn from_sur(file: &[u8]) -> Result<Self, String> {
        let contents = sur::read(file)?;
        let mut payload = vec![NO_LOCAL_PLACE];
        payload.extend(&contents.options_lzma2);
        let dictionary = Dictionary::shared();
        let (_, options) = static_info::parse_payload(&payload, dictionary)
            .map_err(|error| format!("the options: {error}"))?;
        let info = StaticInfo::new(&options, dictionary)
            .map_err(|error| format!("the options: {error}"))?;
        let worlds: Vec<Snapshot> = contents
            .key_frames
            .iter()
            .map(|frame| Snapshot {
                time: frame.time(),
                body: frame.data[4..].to_vec(),
            })
            .collect();
        let mut inputs = Vec::new();
        let mut events = Vec::new();
        for record in &contents.records {
            if record.kind < record_type::FIRST_EVENT {
                continue;
            }
            let event = &record.event;
            let time = event.time;
            for player in 0..sur::MAX_PLAYERS {
                if event.enter & (1 << player) != 0 {
                    events.push((time, player, true));
                }
                if event.leave & (1 << player) != 0 {
                    events.push((time, player, false));
                }
            }
            for &(player, value) in &event.profiles {
                let tick = tick_byte(time);
                inputs.push((time, usize::from(player), Relayed::Profile { tick, value }));
            }
            for &(player, value) in &event.inputs {
                let input = PlayerInput {
                    time: if time % TICK_MS == 0 {
                        WireTime::tick(time)
                    } else {
                        WireTime::precise(time)
                    },
                    rotation: value.rotation,
                    actions: value.actions,
                };
                inputs.push((time, usize::from(player), Relayed::Input(input)));
            }
        }
        let end = contents
            .records
            .last()
            .map_or(0, |record| record.event.time);
        if worlds.is_empty() {
            return Err("no key frame".into());
        }
        Ok(Self {
            info,
            worlds,
            inputs,
            events,
            end,
        })
    }

    /// The key frame a game shown `time` starts from: the last at or before
    /// it, else the first.
    fn world_for(&self, time: u32) -> &Snapshot {
        let after = self.worlds.partition_point(|world| world.time <= time);
        &self.worlds[after.saturating_sub(1)]
    }

    /// How far past the world at `from` a game started from it can be put,
    /// at most `to`: its inputs since the world come with it, read against
    /// the clock it is put at (a tick byte reaches 6.35 s: no more than
    /// [`TICK_REACH_MS`] back), and a game keeps [`INPUT_RING`] of a
    /// player's (one fewer while the world is on its way,
    /// `INPUT_RING_SAFE`); one older than those would be played wrong. So no
    /// player may have more inputs in (`from`, the time and
    /// [`RELAY_LEAD_MS`]].
    fn reach(&self, from: u32, to: u32) -> u32 {
        let to = to.min(from + TICK_REACH_MS);
        let mut counts = [0usize; sur::MAX_PLAYERS];
        let first = self.inputs.partition_point(|(at, _, _)| *at <= from);
        for &(at, player, what) in &self.inputs[first..] {
            if at > to + RELAY_LEAD_MS {
                break;
            }
            if let (Relayed::Input(_), Some(count)) = (what, counts.get_mut(player)) {
                *count += 1;
                if *count > INPUT_RING_SAFE {
                    return to.min(at.saturating_sub(RELAY_LEAD_MS + 1)).max(from);
                }
            }
        }
        to
    }

    /// How many of its inputs and roster changes are due by `time`.
    fn due(&self, time: u32) -> (usize, usize) {
        let horizon = time + RELAY_LEAD_MS;
        (
            self.inputs.partition_point(|(at, _, _)| *at <= horizon),
            self.events.partition_point(|(at, _, _)| *at <= horizon),
        )
    }
}

/// How fast a recording plays.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Speed {
    Half,
    Normal,
    Double,
    Quadruple,
}

impl Speed {
    /// Match ms in `elapsed` of the wall clock.
    fn of(self, elapsed: Duration) -> u32 {
        let ms = elapsed.as_millis() as u32;
        match self {
            Self::Half => ms / 2,
            Self::Normal => ms,
            Self::Double => ms * 2,
            Self::Quadruple => ms * 4,
        }
    }
}

/// A playback control (`SRP_DEV_REPLAY`'s control file, a line each).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Control {
    /// `pause`
    Pause,
    /// `play`
    Play,
    /// `speed 0.5|1|2|4`
    Speed(Speed),
    /// `seek <seconds>` (match time)
    Seek(u32),
}

impl Control {
    pub(crate) fn parse(line: &str) -> Option<Self> {
        let mut words = line.split_whitespace();
        let control = match (words.next()?, words.next()) {
            ("pause", None) => Self::Pause,
            ("play", None) => Self::Play,
            ("speed", Some("0.5")) => Self::Speed(Speed::Half),
            ("speed", Some("1")) => Self::Speed(Speed::Normal),
            ("speed", Some("2")) => Self::Speed(Speed::Double),
            ("speed", Some("4")) => Self::Speed(Speed::Quadruple),
            ("seek", Some(seconds)) => {
                let seconds: f64 = seconds.parse().ok().filter(|s: &f64| *s >= 0.0)?;
                Self::Seek((seconds * 1000.0) as u32)
            }
            _ => return None,
        };
        words.next().is_none().then_some(control)
    }
}

/// The playback clock: match time, by the wall clock.
#[derive(Clone, Copy, Debug)]
enum Clock {
    /// Its game loads its level: the clock waits at `at`.
    Loading {
        at: u32,
    },
    /// At `at` at `since`, running at the playback's speed.
    Playing {
        since: Instant,
        at: u32,
    },
    Paused {
        at: u32,
    },
}

/// What became of a playback.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Ended {
    /// Its game left by its own word, or its link is gone: forgotten.
    Left,
    /// The recording is over: its game is let go (and then forgotten).
    Over,
}

/// How a game came to be shown the recording.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Came {
    /// Its session watches the lobby's match `ticket` (Queue and
    /// spectate): let go when it watches no more, and out of its queue when
    /// it leaves or the recording ends.
    Watch { ticket: u32 },
    /// It pressed Play (the lobby's dev shortcut) and was seated, as
    /// `account_id`, in the match `match_id`, which no game plays here. The
    /// seat is kept while it watches: given up, the lobby would tell its
    /// game the match is over (`85` lobby state, its results), and a game
    /// told that while it loads never ends its load (seen 2026-11-07). The
    /// match is not given up as one nobody came to meanwhile
    /// ([`DevReplay::holds`]); its seat is given up when the game leaves or
    /// the recording ends.
    Play { match_id: u32, account_id: u32 },
}

impl Came {
    fn match_id(self) -> u32 {
        match self {
            Self::Watch { ticket } => ticket,
            Self::Play { match_id, .. } => match_id,
        }
    }
}

/// One spectator's game shown a recording.
pub(crate) struct Playback {
    recording: Arc<Recording>,
    spectator: Spectator,
    came: Came,
    clock: Clock,
    speed: Speed,
    /// How many of the recording's inputs and roster changes have gone to
    /// it as they came due (those since its world go with the world).
    inputs: usize,
    events: usize,
    created: Instant,
    settings: Settings,
    /// `SRP_DEV_REPLAY_LOCAL_PLACE`: the place its 0xE4 makes its own
    /// (none by default): an experiment, whether a game loads the match as
    /// a player where it does not as a spectator.
    local_place: u8,
}

impl Playback {
    fn new(
        recording: Arc<Recording>,
        spectator: Spectator,
        came: Came,
        settings: Settings,
        local_place: u8,
        now: Instant,
    ) -> Self {
        let at = recording.worlds[0].time;
        let (inputs, events) = recording.due(at);
        Self {
            recording,
            spectator,
            came,
            clock: Clock::Loading { at },
            speed: Speed::Normal,
            inputs,
            events,
            created: now,
            settings,
            local_place,
        }
    }

    /// The playback clock.
    fn position(&self, now: Instant) -> u32 {
        match self.clock {
            Clock::Loading { at } | Clock::Paused { at } => at,
            Clock::Playing { since, at } => {
                at + self.speed.of(now.saturating_duration_since(since))
            }
        }
    }

    fn watched<'a>(&self, recording: &'a Recording, clock: u32) -> Watched<'a> {
        Watched {
            id: self.came.match_id(),
            clock,
            info: &recording.info,
            local_place: self.local_place,
            places: usize::from(recording.info.roster()),
            roster: usize::from(recording.info.roster()),
            worlds: recording.worlds.iter().collect(),
            inputs: &recording.inputs[..self.inputs],
            events: &recording.events[..self.events],
            compress: self.settings.compress_state,
            entry_state: self.settings.timing.entry_state,
        }
    }

    /// A datagram of its game: `Some` once its game has left.
    fn receive(&mut self, datagram: &[u8], now: Instant) -> Option<Ended> {
        let recording = Arc::clone(&self.recording);
        let session_id = self.spectator.session_id();
        log::trace!(
            "Replay {session_id:#x}: in {} bytes: {}",
            datagram.len(),
            messages(Direction::ToServer, datagram)
        );
        for event in self.spectator.receive(datagram, now) {
            let Event::Message { id, payload } = event else {
                log::info!("Replay {session_id:#x}: its game disconnected");
                return Some(Ended::Left);
            };
            let before = self.spectator.stage();
            let watched = self.watched(&recording, self.position(now));
            let heard = self.spectator.hear(&watched, id, &payload, now);
            let after = self.spectator.stage();
            if after != before {
                log::debug!(
                    "Replay {session_id:#x}: heard {id:#04x} ({} bytes): {before} -> {after}",
                    payload.len()
                );
            } else if id == survarium::v026g::match_msgs::id::KEEP_ALIVE {
                log::trace!("Replay {session_id:#x}: heard 0xe1 at {before}");
            } else {
                log::debug!(
                    "Replay {session_id:#x}: heard {id:#04x} ({} bytes) at {before}",
                    payload.len()
                );
            }
            match heard {
                Heard::Left => {
                    log::info!("Replay {session_id:#x}: its game left (0xE0)");
                    return Some(Ended::Left);
                }
                // It has every place from its 0xE4: nothing is installed.
                Heard::Installed(_) | Heard::Nothing => {}
            }
        }
        None
    }

    pub(crate) fn control(&mut self, control: Control, now: Instant) {
        let at = self.position(now);
        log::info!(
            "Replay for spectator {:#x}: {control:?} at {at} ms",
            self.spectator.session_id()
        );
        match control {
            Control::Pause => {
                if let Clock::Playing { .. } | Clock::Loading { .. } = self.clock {
                    self.clock = Clock::Paused { at };
                }
            }
            // Its game's clock ran on while paused: it starts again from
            // where the playback stopped.
            Control::Play => {
                if let Clock::Paused { at } = self.clock {
                    self.clock = Clock::Playing { since: now, at };
                    self.seek(at, now);
                }
            }
            Control::Speed(speed) => {
                if let Clock::Playing { .. } = self.clock {
                    self.clock = Clock::Playing { since: now, at };
                }
                self.speed = speed;
            }
            Control::Seek(time) => self.seek(time, now),
        }
    }

    /// The game starts again from the key frame at or before `time`, put as
    /// far past it as it can be.
    fn seek(&mut self, time: u32, now: Instant) {
        let from = self.recording.world_for(time).time;
        let at = self
            .recording
            .reach(from, time.min(self.recording.end).max(from));
        self.clock = match self.clock {
            Clock::Loading { .. } => Clock::Loading { at },
            Clock::Playing { .. } => Clock::Playing { since: now, at },
            Clock::Paused { .. } => Clock::Paused { at },
        };
        (self.inputs, self.events) = self.recording.due(at);
        self.spectator.wait_again();
    }

    /// Its turn: what came due goes, it is started from a world when it
    /// waits for one, and the end of the recording lets it go.
    fn poll(&mut self, now: Instant) -> (Vec<(SocketAddr, Vec<u8>)>, Option<Ended>) {
        let session_id = self.spectator.session_id();
        if self.spectator.timed_out(&self.settings.timing, now) {
            log::info!(
                "Replay {session_id:#x}: its game sent nothing for too long at {}; forgotten",
                self.spectator.stage()
            );
            return (Vec::new(), Some(Ended::Left));
        }
        let before = self.spectator.stage();
        if let Clock::Loading { at } = self.clock
            && self.spectator.has_loaded()
        {
            self.clock = Clock::Playing { since: now, at };
        }
        let at = self.position(now);
        let mut ended = None;
        if at >= self.recording.end && !self.spectator.is_leaving() {
            log::info!(
                "Replay for spectator {:#x}: the recording is over",
                self.spectator.session_id()
            );
            self.spectator.farewell(at, now);
            ended = Some(Ended::Over);
        }
        let recording = Arc::clone(&self.recording);
        let (inputs, events) = recording.due(at);
        for &(time, index, entered) in &recording.events[self.events..events] {
            let message = if entered {
                ServerMessage::PlayerEntered {
                    tick: tick_byte(time),
                    player: index as u8,
                }
            } else {
                ServerMessage::PlayerLeft {
                    tick: tick_byte(time),
                    player: index as u8,
                }
            };
            self.spectator.roster_change(index, entered, time, &message);
        }
        for (time, from, what) in &recording.inputs[self.inputs..inputs] {
            self.spectator
                .relay(*from, &relay(*from, what), *time, at, now);
        }
        (self.inputs, self.events) = (inputs, events);
        let echo = now.saturating_duration_since(self.created).as_millis() as u16;
        let start = recording.world_for(at).time;
        self.spectator
            .step(&self.watched(&recording, at), Some(start), echo, now);
        let after = self.spectator.stage();
        if after != before {
            log::debug!("Replay {session_id:#x}: {before} -> {after} at {at} ms");
        }
        let datagrams = self.spectator.poll(now);
        for (_, datagram) in &datagrams {
            log::trace!(
                "Replay {session_id:#x}: out {} bytes: {}",
                datagram.len(),
                messages(Direction::ToClient, datagram)
            );
        }
        (datagrams, ended)
    }
}

/// A datagram's messages for the log: id, part and length.
fn messages(direction: Direction, datagram: &[u8]) -> String {
    match wire::parse(
        Table::Match,
        direction,
        datagram,
        mask_bytes(sur::MAX_PLAYERS as u8),
    ) {
        Ok(parsed) if parsed.messages.is_empty() => "acks only".to_owned(),
        Ok(parsed) => parsed
            .messages
            .iter()
            .map(|message| match message.part {
                Some(part) => format!(
                    "{:02X} part {} ({} bytes)",
                    message.id,
                    part.index,
                    message.payload.len()
                ),
                None => format!("{:02X} ({} bytes)", message.id, message.payload.len()),
            })
            .collect::<Vec<_>>()
            .join(", "),
        Err(error) => format!("unparsed: {error:?}"),
    }
}

/// `SRP_DEV_REPLAY`: every spectator's game is shown the recording, each
/// on its own playback; the controls in `<recording>.control` (a line
/// each, read and removed) go to all of them.
pub(crate) struct DevReplay {
    recording: Arc<Recording>,
    /// `SRP_DEV_REPLAY_LOCAL_PLACE` ([`Playback::local_place`]).
    local_place: u8,
    /// `SRP_DEV_REPLAY_PLAY_LIVE`: a game seated by Play plays its match
    /// (alone) instead of watching the recording: a control, whether a
    /// match the lobby makes that way loads at all.
    play_live: bool,
    /// The match port (0xE3), and the server's settings.
    port: u16,
    settings: Settings,
    control: PathBuf,
    next_control: Instant,
    playbacks: HashMap<SocketAddr, Playback>,
}

impl DevReplay {
    pub(crate) fn load(
        path: &Path,
        port: u16,
        settings: &Settings,
        now: Instant,
    ) -> Result<Self, String> {
        let recording = Recording::load(path)?;
        let mut control = path.as_os_str().to_owned();
        control.push(".control");
        Ok(Self {
            recording: Arc::new(recording),
            local_place: std::env::var("SRP_DEV_REPLAY_LOCAL_PLACE")
                .ok()
                .and_then(|place| place.trim().parse().ok())
                .unwrap_or(NO_LOCAL_PLACE),
            play_live: std::env::var_os("SRP_DEV_REPLAY_PLAY_LIVE").is_some(),
            port,
            settings: settings.clone(),
            control: control.into(),
            next_control: now,
            playbacks: HashMap::new(),
        })
    }

    pub(crate) fn describe(&self) -> String {
        let recording = &self.recording;
        format!(
            "{} places, {} ms, {} key frames, {} inputs, its own place {:#04x}; controls in {}",
            recording.info.roster(),
            recording.end,
            recording.worlds.len(),
            recording.inputs.len(),
            self.local_place,
            self.control.display()
        )
    }

    /// The addresses of the games shown the recording.
    pub(crate) fn addrs(&self) -> impl Iterator<Item = SocketAddr> + '_ {
        self.playbacks.keys().copied()
    }

    /// Whether a game seated by Play watches the recording
    /// ([`DevReplay::play_live`]).
    pub(crate) fn takes_play(&self) -> bool {
        !self.play_live
    }

    pub(crate) fn owns(&self, addr: SocketAddr) -> bool {
        self.playbacks.contains_key(&addr)
    }

    /// The game at `addr` of `session_id` is shown the recording as a
    /// spectator, in place of what its session `came` by.
    pub(crate) fn connect(
        &mut self,
        addr: SocketAddr,
        session_id: u32,
        came: Came,
        transport: Transport,
        now: Instant,
    ) {
        log::info!(
            "Replay: the game of session {session_id:#x} at {addr} ({came:?}) is shown the \
             recording"
        );
        self.playbacks
            .retain(|_, playback| playback.spectator.session_id() != session_id);
        let spectator = Spectator::connect(
            addr,
            session_id,
            transport,
            sur::MAX_PLAYERS as u8,
            self.port,
        );
        let playback = Playback::new(
            Arc::clone(&self.recording),
            spectator,
            came,
            self.settings.clone(),
            self.local_place,
            now,
        );
        self.playbacks.insert(addr, playback);
    }

    /// A datagram of the game at `addr`: the session of a game that left by
    /// itself, and how it came, for the lobby to be told.
    pub(crate) fn receive(
        &mut self,
        addr: SocketAddr,
        datagram: &[u8],
        now: Instant,
    ) -> Option<(u32, Came)> {
        let playback = self.playbacks.get_mut(&addr)?;
        // One let go is silent until it is forgotten.
        if playback.spectator.is_leaving() {
            return None;
        }
        playback.receive(datagram, now)?;
        let playback = self.playbacks.remove(&addr)?;
        Some((playback.spectator.session_id(), playback.came))
    }

    /// Whether a game seated in the match `match_id` watches the recording
    /// (or is being let go).
    pub(crate) fn holds(&self, match_id: u32) -> bool {
        self.playbacks.values().any(
            |playback| matches!(playback.came, Came::Play { match_id: id, .. } if id == match_id),
        )
    }

    pub(crate) fn control(&mut self, control: Control, now: Instant) {
        for playback in self.playbacks.values_mut() {
            playback.control(control, now);
        }
    }

    /// Each playback's turn. `watching` says whether a session still
    /// watches a match (the lobby's; a watcher that does not is let go);
    /// `left` hears of each game that left or saw the end, and how it came,
    /// for the lobby to be told.
    pub(crate) fn poll(
        &mut self,
        now: Instant,
        watching: impl Fn(u32, u32) -> bool,
        mut left: impl FnMut(u32, Came),
    ) -> Vec<(SocketAddr, Vec<u8>)> {
        if now >= self.next_control {
            self.next_control = now + CONTROL_EVERY;
            self.read_controls(now);
        }
        let mut datagrams = Vec::new();
        self.playbacks.retain(|_, playback| {
            let session_id = playback.spectator.session_id();
            let ticket = match playback.came {
                Came::Watch { ticket } => Some(ticket),
                Came::Play { .. } => None,
            };
            if let Some(ticket) = ticket
                && !playback.spectator.is_leaving()
                && !watching(session_id, ticket)
            {
                let at = playback.position(now);
                playback.spectator.farewell(at, now);
            }
            let (out, ended) = playback.poll(now);
            datagrams.extend(out);
            let kept = ended != Some(Ended::Left) && !playback.spectator.is_forgotten(now);
            // A watcher is out of its queue at once; a seat goes once the
            // game is gone (its farewell taken, or its silence over), so the
            // lobby tells it nothing while it is in the match.
            match playback.came {
                Came::Watch { .. } if ended.is_some() => left(session_id, playback.came),
                Came::Play { .. } if !kept => left(session_id, playback.came),
                Came::Watch { .. } | Came::Play { .. } => {}
            }
            kept
        });
        datagrams
    }

    fn read_controls(&mut self, now: Instant) {
        let Ok(text) = std::fs::read_to_string(&self.control) else {
            return;
        };
        let _ = std::fs::remove_file(&self.control);
        for line in text.lines().filter(|line| !line.trim().is_empty()) {
            match Control::parse(line) {
                Some(control) => self.control(control, now),
                None => log::warn!("Replay: no such control: {line:?}"),
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn position(&self, session_id: u32, now: Instant) -> Option<u32> {
        self.playbacks
            .values()
            .find(|playback| playback.spectator.session_id() == session_id)
            .map(|playback| playback.position(now))
    }
}
