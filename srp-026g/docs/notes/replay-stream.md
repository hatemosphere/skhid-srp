# A recording played to a spectator (spike, dev only)

**Version scope:** 0.26g, on top of Queue and spectate (#1285, `queue-spectate.md`). Written
2026-11-07. **Status: server side only, tested with scripted clients; never run against a game.**
Path A of `research/replay-plan.md`: the server streams a recording as a spectated match, so a
replay plays in the running game (lobby -> replay -> lobby, no second process).

## What it does

- `SRP_DEV_REPLAY=<file.sur | file.srprec>` (a `.srprec` is converted on load, as
  `srp-recording-to-sur` does, with no mode config): every game that connects as a spectator
  (`Queue and spectate`, a client with the late-join capability) is shown the recording instead of
  the match it asked for, each on its own playback (`game/playback.rs`).
- It goes through #1285's spectator machine (`game/spectate.rs`, `Watch`), which now reads a
  `Watched` view: a live `Game`, or the recording as of its playback clock. Same handshake (0xE3,
  0xE4 naming 0xFF with every place of the recording, 0xDA), same start (0xE5 of the next
  generation until 0xDB, then the inputs and entries since the world, 0xE5, 0xE8), same relays
  and roster changes once live, same farewell (0xEA naming 0xFF, then 7 s of silence).
- The worlds are the `.sur`'s key frames; the relays its inputs (0xEB/0xEC), kit choices (0xF3),
  entries and leaves, at their recorded times, 250 ms ahead of the clock (a live game gets an
  input about that early).
- The clock waits while the game loads its level, then runs from the first key frame. The end
  of the recording lets the game go and takes it out of its queue (else the lobby would send it to
  "watch" again).
- Controls, a line each in `<SRP_DEV_REPLAY>.control` (read every 250 ms and removed; to every
  playback): `pause`, `play`, `speed 0.5|1|2|4`, `seek <seconds>`. No follow: #1285's is the
  game's own (F5/F6, F2).
  - `pause` stops the clock (nothing more is sent).
  - `speed` scales the clock.
  - `seek t` starts the game again from the key frame K at or before t, its clock put at the
    furthest point <= t it can be: no more than 5 s past K (a tick byte reaches 6.35 s back) and
    no player with more than 26 inputs in between (`INPUT_RING_SAFE`; the game keeps 27). The
    inputs since K go with the world: the game plays K..clock at once on load.
  - `play` after a pause is a seek to where it stopped (the game's clock ran on meanwhile).

## Not known until a game runs it (predictions)

1. **Pause.** The game's clock is not ours: 0xE5 sets it (`game::set_time_in_ms` 0x1EE4A0) and it
   runs on; `move` 0x2D2E40 plays 50 ms ticks up to it (`client_patch.md` "What depends on the
   factor"). So while paused the world goes on with no new inputs, each player on its last input
   (LIKELY: its actions mask held, so a runner keeps running), and the camera follows that
   drift. In the tests the spectator's clock was 5.5 s past the paused playback after 5.5 s.
   Resuming without a resync would send inputs past its horizon: moved to a tick of its own
   ("input is moved", `match.md` §5), and after 6.35 s read at wrong ticks. Hence `play` =
   resync. A real freeze needs an exe patch: `game::pause` 0x1ED5C0 (inferred-high) on a
   spectator control message, or the timer's speed (below) at 0.
2. **Speed.** Not with a stock game. Its clock runs at 1x; only 0xE7 steers it, at
   1 +- `time_floating_factor` (0.05, 0.3 from revision 8, at most 0.5; `client_patch.md`
   `set_offset` 0x1EE9E0), so 0.7x..1.3x with today's patch, never 0.5x/2x/4x. At 2x the relays
   run ahead of the game's clock by 1 s a second: past 2.5 s (5 x horizon) a relayed input is
   moved back to under that limit (PROVEN, `match.md` §5), so the world goes wrong after about
   2.5 s; past 6.35 s ticks are misread. At 0.5x they arrive late: moved forward after 0.5 s
   (horizon). The scripted client cannot show this (its model reads a tick byte against its
   clock and has no "too far" rule); in the tests at 4x its clock was 16 s behind. What would work:
   an exe patch setting the game timer's speed (`floating_timer` +0x20, the `elapsed x [+0x20]`
   term, `client_patch.md` 0x147110) from a spectator control message, the replay viewer's
   `time_scale` (`replay_sur.md`) for the online game, with the server scaling 0xE5/relays as here.
3. **Seek (mid-match resync).** LIKELY to work: it is the very path a spectator out of step and a
   player resynced while playing take (0xE5 of a new generation clears the history, 0xE8 loads
   with "FULL RESYNC", PROVEN live for players, `match.md` §30; #1285's spectator restart). New
   for a seek: going **back** in time (the game's clock moves back; for players the server never
   does this, "a game whose clock went back made its inputs of those ticks once more", but a
   spectator makes no inputs, patch 103), a jump of minutes forward, and roster changes the game
   already took before (an entry after K comes again). UNKNOWN: the HUD's kill feed, scores and
   timers across a backward jump; whether a world 5 s behind the clock catches up smoothly (the
   game simulates the 5 s on load, 100 ticks).
4. **Other unknowns.** Bit 31 of an input's actions (a per-input flag the client splits off) is
   cleared by the converter, so the relays lack it (`sur_convert.rs` `INPUT_FLAG`); whether that
   changes a world online is not known. The 0xE4 holds every place from the start, as the replay
   viewer has them, with key frames before a late install padded (`sur_convert.rs`); untested
   online. The worlds a seek can start from are the recording's uploads, about 5 s apart (a
   running match asks for one every 5 s; a `.sur` made by the CLI keeps one per 10 s), so a seek
   lands up to that much before t when the inputs since K stop it short.

## How to try it

On the dev server: `SRP_DEV_REPLAY=/tmp/srp-local/recordings/<file>.srprec`, a room whose match
runs, "Queue and spectate" with a patched game (#1285's patch 103). Then, for example,
`echo "seek 60" > /tmp/srp-local/recordings/<file>.srprec.control`. The game must run the same
simulation as the recording (`simulation_changes.tsv`).
