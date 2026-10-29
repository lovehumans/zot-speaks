//! The audio sink: one player, `ffplay`, fed raw PCM through a pipe.
//!
//! Everything this extension knows about the player lives on this side of the seam: the argv that
//! states the format, the gain scaling and s16le conversion that produce the bytes, the clock that
//! keeps the feed at the player's pace, what the player's own words mean when it gives up, and how it
//! is shut down — drained if the audio is still wanted, killed after a bounded grace if it is not.
//! The caller owns the utterance: what to synthesize, when a Stop or a superseding request cuts it
//! off, and which sentence reaches the user.
//!
//! Three verbs, in order: [`Sink::open`], [`Sink::write`], [`Sink::finish`]. Nothing else is public,
//! so the rules below are the only way to talk to a player, and each one is testable where it lives.

use std::io::{self, Read, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::settings::Speed;

/// The audio prerequisite: `ffplay`, which ships with ffmpeg. It reads raw s16le mono from a pipe on
/// every platform this extension runs on, and it is the player with a tempo knob (`atempo`), which is
/// what makes the Speed setting mean something. `paplay` was dropped 2026-10-29 (ADR-0019): a second
/// player meant a capability ladder that could silently discard the speed you had set. `afplay` stays
/// deliberately absent: it wants a file, so using it would mean buffering the whole utterance first.
pub const PLAYER: &str = "ffplay";

/// What a box without that player is told. The fix is an install, so it says install, and it says it
/// the same way on every platform: which package manager to use is the user's business.
const NO_PLAYER_HINT: &str =
    "no audio player found: install ffmpeg so that `ffplay` is on your PATH";

/// How far ahead of real-time playback we run: small enough that a stop goes quiet within a
/// chunk, large enough that the player never starves.
const PLAY_LEAD: Duration = Duration::from_millis(40);

/// How long a sink cut short may keep draining before we kill it: `ffplay` sits on an SDL audio
/// buffer, so this is what bounds the tail of a stopped utterance.
const STOP_GRACE: Duration = Duration::from_millis(150);
const STOP_GRACE_TICK: Duration = Duration::from_millis(10);

/// Lines a player prints when it could not play at all. Measured with ffplay n9.0.2: an unopenable
/// audio device, an unreadable input and an unknown filter each exit with status **0** (only an
/// unrecognised argument exits 1), so a fatal `ffplay` run can look exactly like a successful one
/// and these lines are the only report it gives.
///
/// Deliberately narrow, because a healthy run is not quiet either: pipewire's `scandir … Permission
/// denied` and the ALSA library's complaints arrive on the same stderr and must not read as failure.
const FATAL_PLAYER_LINES: [&str; 4] = [
    "failed to open",
    "audio open failed",
    "no such filter",
    "error processing filtergraph",
];

/// A player that never started. Both shapes are reported, and they are different diagnoses: one
/// tells the user to install ffmpeg, the other tells them the ffmpeg they have is unusable.
pub enum SinkError {
    /// `ffplay` is not on `PATH`.
    MissingPlayer,
    /// It is there and could not be executed (no exec bit, bad interpreter, …).
    CouldNotStart { detail: String },
    /// The process started and has no stdin to feed. Cannot happen with `Stdio::piped()`; if it ever
    /// does, the child is reaped here rather than left behind.
    NoStdin,
}

impl SinkError {
    /// The sentence for the user, without the `zot-speaks: ` prefix the extension puts on everything.
    pub fn report(&self) -> String {
        match self {
            SinkError::MissingPlayer => NO_PLAYER_HINT.to_string(),
            SinkError::CouldNotStart { detail } => {
                format!("could not start {PLAYER}: {detail}")
            }
            SinkError::NoStdin => format!("no stdin for {PLAYER}"),
        }
    }
}

/// What a failed `spawn` means. Pure, so both sentences are testable without a subprocess — the
/// distinction is `NotFound` and nothing else, which is why this module never searches `PATH` itself.
fn spawn_problem(err: &io::Error) -> SinkError {
    if err.kind() == io::ErrorKind::NotFound {
        SinkError::MissingPlayer
    } else {
        SinkError::CouldNotStart {
            detail: err.to_string(),
        }
    }
}

/// How the player is told the format: mono s16le on stdin at this rate, read from the pipe. The speed
/// is asked of the player, never of the model (ADR-0012) — `atempo` changes the rate without changing
/// the pitch, and it has to sit before the `-`, because ffmpeg reads it as an option on the input that
/// follows it, and the input is the pipe.
fn player_args(rate: u32, speed: Speed) -> Vec<String> {
    let mut args = vec![
        "-nodisp".into(),
        "-autoexit".into(),
        "-loglevel".into(),
        "error".into(),
        "-f".into(),
        "s16le".into(),
        "-ar".into(),
        rate.to_string(),
        "-ch_layout".into(), // not `-ac 1`: removed in newer ffmpeg
        "mono".into(),
    ];
    if let Some(tempo) = speed.tempo_arg() {
        args.push("-af".into());
        args.push(format!("atempo={tempo}"));
    }
    args.push("-".into());
    args
}

/// One mono sample, in the format the player is told to expect. The gain is `Settings::gain` — twice
/// the volume dial — so a loud voice has to clamp rather than wrap: an i16 that overflows plays as a
/// click. A non-finite sample is silence, not NaN cast to an arbitrary integer.
fn sample_to_s16(sample: f32, gain: f32) -> i16 {
    let value = if sample.is_finite() {
        (sample * gain).clamp(-1.0, 1.0)
    } else {
        0.0
    };
    (value * 32767.0).round().clamp(-32768.0, 32767.0) as i16
}

/// How long the writer must wait after having fed this many samples. The player drains what it is
/// handed `tempo` times faster than natural time, so feeding at natural time starves it; `1.0x` is
/// exactly real time. Never negative, and no wait at all once the feed is behind the clock.
fn feed_pause(samples: u64, rate: u32, tempo: f64, elapsed: Duration) -> Option<Duration> {
    let audio = Duration::from_secs_f64(samples as f64 / (rate as f64 * tempo));
    (audio > elapsed + PLAY_LEAD).then(|| audio - elapsed - PLAY_LEAD)
}

/// Why a player's run ended badly, in that player's own words, or None when it ended well.
///
/// `ffplay` does not report failure through its exit status (see `FATAL_PLAYER_LINES`), so a fatal
/// line counts whatever the status claims; a run that failed without naming a reason still gets its
/// first stderr line, which is the best report available.
fn player_failure_reason(exit_ok: bool, stderr: &str) -> Option<String> {
    if let Some(line) = stderr.lines().map(str::trim).find(|line| {
        let lower = line.to_lowercase();
        FATAL_PLAYER_LINES
            .iter()
            .any(|marker| lower.contains(marker))
    }) {
        return Some(line.to_owned());
    }
    (!exit_ok).then(|| {
        stderr
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .unwrap_or("nothing on stderr")
            .to_owned()
    })
}

/// A player running, fed, and ready to be told the format.
pub struct Sink {
    child: Child,
    stdin: ChildStdin,
    /// Reused so a chunk costs no allocation; `write` clears it before filling it.
    buffer: Vec<u8>,
    gain: f32,
    rate: u32,
    tempo: f64,
    started: Instant,
    samples: u64,
}

impl Sink {
    /// Start the player for one utterance: this is the rate the synthesis produced, the speed the
    /// settings asked for, and the gain the volume dial means. Everything the byte stream depends on
    /// is fixed here, which is what leaves [`Sink::write`] one argument wide.
    pub fn open(rate: u32, speed: Speed, gain: f32) -> Result<Sink, SinkError> {
        let mut child = match Command::new(PLAYER)
            .args(player_args(rate, speed))
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(child) => child,
            Err(e) => return Err(spawn_problem(&e)),
        };
        let Some(stdin) = child.stdin.take() else {
            let _ = child.kill();
            let _ = child.wait();
            return Err(SinkError::NoStdin);
        };
        Ok(Sink {
            child,
            stdin,
            buffer: Vec::new(),
            gain,
            rate,
            tempo: speed.factor(),
            started: Instant::now(),
            samples: 0,
        })
    }

    /// Convert one mono chunk to the player's format and hand it over, then wait out the share of
    /// real time it represents. An `Err` means the player is gone; the utterance is over, and
    /// [`Sink::finish`] is still what reaps it.
    pub fn write(&mut self, samples: &[f32]) -> io::Result<()> {
        self.buffer.clear();
        self.buffer.reserve(samples.len() * 2);
        for &sample in samples {
            self.buffer
                .extend_from_slice(&sample_to_s16(sample, self.gain).to_le_bytes());
        }
        self.stdin.write_all(&self.buffer)?;
        self.samples += samples.len() as u64;
        if let Some(pause) = feed_pause(self.samples, self.rate, self.tempo, self.started.elapsed())
        {
            thread::sleep(pause);
        }
        Ok(())
    }

    /// End the utterance: close our stdin, which is EOF for the player. Audio the user still wants
    /// gets to drain whatever the player holds. Audio they have moved on from gets a bounded grace and
    /// is then killed rather than orphaned.
    ///
    /// A player that gave up on its own has something to say, and its words are the report whatever
    /// its exit status claims (see [`FATAL_PLAYER_LINES`]); one we killed on purpose has nothing, and
    /// is not asked. stdin is already closed, so reading stderr to EOF after the child is reaped
    /// cannot block.
    pub fn finish(self, cut_short: bool) -> Finished {
        let Sink {
            mut child,
            stdin,
            samples,
            ..
        } = self;
        drop(stdin);
        let mut killed = false;
        if cut_short {
            let deadline = Instant::now() + STOP_GRACE;
            while child.try_wait().ok().flatten().is_none() && Instant::now() < deadline {
                thread::sleep(STOP_GRACE_TICK);
            }
            killed = true;
            let _ = child.kill();
        }
        let mut failure = None;
        if !killed {
            let exit_ok = matches!(child.wait(), Ok(status) if status.success());
            let mut stderr = String::new();
            if let Some(mut pipe) = child.stderr.take() {
                let _ = pipe.read_to_string(&mut stderr);
            }
            failure = player_failure_reason(exit_ok, &stderr)
                .map(|reason| format!("{PLAYER} stopped: {reason}"));
        }
        Finished { samples, failure }
    }
}

/// What a finished utterance left behind, for the caller to act on.
pub struct Finished {
    /// Zero when the player produced nothing at all, which is how a caller tells "this line has no
    /// audio" from "this player is broken".
    pub samples: u64,
    /// What the player said when it gave up, already a sentence. None when it ended well, and None
    /// when we killed it, because a kill we asked for is not a failure.
    failure: Option<String>,
}

impl Finished {
    pub fn silent(&self) -> bool {
        self.samples == 0
    }

    pub fn failure(&self) -> Option<&str> {
        self.failure.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        feed_pause, player_args, player_failure_reason, sample_to_s16, spawn_problem, SinkError,
        NO_PLAYER_HINT, PLAYER,
    };
    use crate::settings::{Speed, DEFAULT_SPEED, SPEED_MAX, SPEED_MIN};
    use std::io::ErrorKind;
    use std::time::Duration;

    /// The whole contract with the player is the format: mono s16le on stdin at the stream's own
    /// rate, read from the pipe. At the natural speed it gets no word about tempo — `1.0x` is its
    /// default — and every rung off it asks for the tempo in the same place: an option on the input.
    #[test]
    fn the_player_is_told_the_pcm_contract_and_the_tempo_before_the_pipe() {
        assert_eq!(
            player_args(24000, DEFAULT_SPEED),
            [
                "-nodisp",
                "-autoexit",
                "-loglevel",
                "error",
                "-f",
                "s16le",
                "-ar",
                "24000",
                "-ch_layout",
                "mono",
                "-",
            ]
        );
        // Every rung off the natural rate gets a filter, and it sits before the `-` because ffmpeg
        // reads `-af` as an option on the input that follows it. The range ffmpeg documents for
        // `atempo` (0.5–2.0) is exactly `SPEED_MIN..=SPEED_MAX`, so the setting cannot ask for
        // something the filter would refuse.
        for tenths in [SPEED_MIN.tenths(), 9, 11, 15, SPEED_MAX.tenths()] {
            let speed = Speed::from_tenths(tenths);
            let joined = player_args(24000, speed).join(" ");
            assert_eq!(
                speed.tempo_arg().as_deref(),
                Some(format!("{}.{}", tenths / 10, tenths % 10).as_str()),
                "{tenths}/10 must name the tempo the argv then carries"
            );
            assert!(
                joined.contains("-af atempo=") && joined.ends_with('-'),
                "{tenths}/10 speed must put the filter on the pipe: {joined}"
            );
            assert!(
                joined.ends_with(&format!("-af atempo={} -", speed.tempo_arg().unwrap())),
                "the filter is an option on the pipe, not a separate argument: {joined}"
            );
        }
        // The ceiling of the ladder is the ceiling of the filter: 2.0 is spoken, `atempo=2.1` never is.
        assert_eq!(
            player_args(24000, Speed::from_tenths(35)).join(" "),
            player_args(24000, SPEED_MAX).join(" "),
            "an off-ladder rung is clamped before it reaches ffmpeg"
        );
    }

    /// The one sentence for a box with no player has to name what to install and the binary that has
    /// to appear, because that is the whole fix. It names no package manager — which one to use is
    /// the user's business — and no player this extension no longer uses.
    #[test]
    fn a_missing_player_is_told_what_to_install() {
        assert_eq!(
            NO_PLAYER_HINT,
            "no audio player found: install ffmpeg so that `ffplay` is on your PATH"
        );
        assert_eq!(SinkError::MissingPlayer.report(), NO_PLAYER_HINT);
        assert!(!NO_PLAYER_HINT.contains("paplay"), "paplay is gone");
        assert!(
            !NO_PLAYER_HINT.contains("brew") && !NO_PLAYER_HINT.contains("apt"),
            "the wording must not guess the user's package manager"
        );
    }

    /// "Not installed" and "installed and unusable" are different problems with different fixes, and
    /// `spawn` already knows which one it hit. Nothing here searches `PATH` first: a pre-spawn probe
    /// would be a second, less reliable answer to the same question.
    #[test]
    fn a_broken_install_is_not_called_a_missing_one() {
        let missing = spawn_problem(&std::io::Error::from(ErrorKind::NotFound));
        assert!(matches!(missing, SinkError::MissingPlayer));
        assert!(missing.report().contains("install ffmpeg"));

        for kind in [ErrorKind::PermissionDenied, ErrorKind::InvalidInput] {
            let broken = spawn_problem(&std::io::Error::from(kind));
            let report = broken.report();
            assert!(matches!(broken, SinkError::CouldNotStart { .. }));
            assert!(
                report.contains(PLAYER),
                "name the player that failed: {report}"
            );
            assert!(
                !report.contains("no audio player found"),
                "a broken install is not a missing one: {report}"
            );
        }
    }

    /// The gain is twice the volume dial, so the multiplication has to clamp instead of wrap: an
    /// i16 that overflows is a click, and a NaN cast is a full-scale value out of nowhere.
    #[test]
    fn pcm_scaling_amplifies_without_wrapping() {
        let quiet = sample_to_s16(0.25, 0.2);
        assert_eq!(quiet, (0.25f64 * 0.2 * 32767.0).round() as i16);
        assert_eq!(
            sample_to_s16(1.0, 2.0),
            32767,
            "a hot sample clamps at full scale"
        );
        assert_eq!(sample_to_s16(-1.0, 2.0), -32767);
        assert_eq!(sample_to_s16(0.0, 2.0), 0);
        assert_eq!(
            sample_to_s16(f32::NAN, 2.0),
            0,
            "a non-finite sample is silence"
        );
        assert_eq!(sample_to_s16(f32::INFINITY, 1.0), 0);
    }

    /// The clock follows the speed asked of the player: audio handed to `atempo` drains that much
    /// faster than natural time, so feeding it at natural time starves the pipe. And a feed that is
    /// already behind never waits — a lagging writer catching up by sleeping would only lag more.
    #[test]
    fn the_feed_keeps_its_own_pace_and_never_sleeps_when_behind() {
        let rate = 24000;
        let one_second = Duration::from_secs(1);
        let lead = Duration::from_millis(40);

        // 1.0x: one second of audio written at t=0 is due one second from now, minus the lead.
        let at_natural = feed_pause(24000, rate, 1.0, Duration::ZERO).unwrap();
        assert!(
            (at_natural - (one_second - lead)).as_millis() < 2,
            "1.0x paces at real time: {at_natural:?}"
        );
        // 2.0x: the same audio is due in half the time, because the player drains it that fast.
        let at_double = feed_pause(24000, rate, 2.0, Duration::ZERO).unwrap();
        assert!(
            at_double.as_millis() < (one_second - lead).as_millis() / 2 + 2,
            "2.0x drains twice as fast, so the writer waits half as long: {at_double:?}"
        );
        assert!(at_double < at_natural);
        // Ahead of the clock already: no wait.
        assert!(feed_pause(24000, rate, 1.0, one_second).is_none());
        assert!(feed_pause(24000, rate, 1.0, one_second * 3).is_none());
        assert!(feed_pause(0, rate, 1.0, Duration::ZERO).is_none());
    }

    /// A failed `ffplay` exits 0. Measured with n9.0.2: no audio device, an unreadable input and an
    /// unknown filter all exit successfully — only an unrecognised argument exits 1 — so its exit
    /// status cannot be the report and its own words have to be. The warnings that accompany a run
    /// that really did play must not turn into errors the user has to read.
    #[test]
    fn what_a_player_says_decides_and_its_exit_status_does_not() {
        // Verbatim shape of `ffplay` on this box with no usable audio device, exiting 0.
        let dead_device =
            "[W] pw.conf | scandir /etc/pipewire/client.conf.d failed: Permission denied\n\
             ALSA lib pcm.c:2722:(snd_pcm_open_noupdate) [error.pcm] Unknown PCM default\n\
             No more combinations to try, audio open failed\n\
             Failed to open file 'fd:' or configure filtergraph";
        assert_eq!(
            player_failure_reason(true, dead_device).as_deref(),
            Some("No more combinations to try, audio open failed"),
            "status 0 is not success for this player"
        );
        let bad_filter =
            "[AVFilterGraph @ 0x7f30] Error processing filtergraph: Filter not found\n\
             Failed to open file 'fd:' or configure filtergraph";
        assert_eq!(
            player_failure_reason(true, bad_filter).as_deref(),
            Some("[AVFilterGraph @ 0x7f30] Error processing filtergraph: Filter not found"),
        );
        // The same noise from a run that played everything: not a failure.
        let healthy =
            "[W] pw.conf | scandir /etc/pipewire/client.conf.d failed: Permission denied\n\
             ALSA lib pcm.c:2722:(snd_pcm_open_noupdate) [error.pcm] Unknown PCM default";
        assert_eq!(player_failure_reason(true, healthy), None);
        assert_eq!(player_failure_reason(true, ""), None);
        // A non-zero exit still gets its report, with or without a line to say why.
        assert_eq!(
            player_failure_reason(false, "Connection failure: Connection refused").as_deref(),
            Some("Connection failure: Connection refused"),
        );
        assert_eq!(
            player_failure_reason(false, "").as_deref(),
            Some("nothing on stderr")
        );
    }
}
