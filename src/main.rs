//! zot-speaks — speak the last agent message aloud via Gradium Phonon.
//!
//! Protocol (JSON-lines on stdin/stdout), matching zot's extension docs:
//!   hello (extension-initiated) -> hello_ack -> register_command tts + tts-settings
//!     + subscribe assistant_message + turn_end + ready
//!   assistant_message events -> cache that session's latest `text`
//!   turn_end with a final stop reason -> speak the message of the session that ended, in
//!     projects with auto-speak on
//!   command_invoked `tts [text]` speaks; `tts stop` cuts it off
//!   command_invoked `tts-settings` opens an interactive panel (voice/volume/temperature/auto-speak);
//!     the panel answers for whichever project the session is currently in
//!   panel_key / panel_close drive the panel; settings persist to <data_dir>/settings.json
//!     moving the voice picker's cursor auditions that voice, `s` speaks the last message in it,
//!     and every new request cuts off the audition it supersedes
//!   queued requests are coalesced, so held keys audition only where the cursor lands
//!   the Temperature row's explanation is the one preview spoken on repeat: it goes on while the row
//!     holds the cursor, re-reads the live settings before each repetition, and ends on the next
//!     utterance, a Silence, or a close
//!   shutdown -> stop, shutdown_ack, exit
//!
//! Playback design (one contract, one code path — the sink, `zot_speaks::sink`):
//!   * `stream_with` yields mono f32 PCM chunks, so time-to-first-audio is decoupled from length.
//!   * The sink owns everything that reaches the player. `Sink::open` starts `ffplay` and states the
//!     format (raw s16le mono on stdin, the speed asked of the player through ffmpeg's `atempo`
//!     filter rather than re-timed here); `Sink::write` scales a chunk by the volume dial
//!     (`Settings::gain`), converts it to s16le and feeds it at the player's pace; `Sink::finish`
//!     drains or kills the player and turns what it said into a sentence. What is left here owns the
//!     utterance: what to synthesize, and when a Stop or a superseding request ends it.
//!   * Voice and temperature go through `SpeechOptions`, resolved against the locally-registered
//!     voice catalog, and every utterance is synthesized from its own noise seed (`tts::SeedPlan`;
//!     `ZOT_SPEAKS_SEED` pins the sequence) so a repeated line is a new take, not the same one.
//!     Stop is cooperative: we drop the `SpeechStream` and close the sink's stdin.
//!   * Pacing to real time lives in the sink, so the player never buffers far ahead — low TTFB,
//!     short tail.

mod panel;
mod wire;

use std::collections::HashMap;
use std::io::{self, BufRead};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use ptts::synth::Synth;
use serde_json::{json, Value};

use zot_speaks::cache;
use zot_speaks::settings::{self, Settings};
use zot_speaks::sink::Sink;
use zot_speaks::tts;
use zot_speaks::voices::{self, VoiceInfo};

use panel::{KeyOutcome, Preview, SettingsPanel, PANEL_ID};
use wire::Out;

const NAME: &str = "zot-speaks";
/// One version per artifact: the package version in `Cargo.toml`, which `dev/test.sh` checks
/// against the version in `extension.json` before it accepts the hello frame.
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The breath between two repetitions of the same line, so a repeated explanation is a sentence you
/// can follow rather than a stutter. Synthesis time adds to it; this is the floor.
const LOOP_GAP: Duration = Duration::from_millis(400);

/// One request to the playback thread.
enum Cmd {
    /// Speak `text` once, with this exact settings snapshot.
    Play {
        text: String,
        settings: Settings,
    },
    /// Speak `text` in `voice` and go on speaking it. The thread re-reads the live settings before
    /// every repetition, so the value the user is looking at is the value the next one is spoken at.
    /// This is the `/tts-settings` panel explaining the Temperature row while it is selected.
    Loop {
        text: String,
        voice: String,
    },
    /// Cut off whatever is playing and go quiet.
    Stop,
    Exit,
}

/// What the playback thread should do next, as decided by an utterance and the queue behind it.
enum Resume {
    /// Go quiet: a Stop had the last word, or the line was not playable at all (no player, a player
    /// that gave up, no audio in it). A repeat request ends here too, so nothing retries on repeat.
    Idle,
    /// The line ran to its end and nothing else is queued. Only a repeat request acts on this; a
    /// one-shot utterance goes quiet.
    Finished,
    /// Speak this once: the newest request in the queue was a one-shot.
    Play {
        text: String,
        settings: Settings,
    },
    /// Speak this again and again: the newest request in the queue was a repeat request.
    Loop {
        text: String,
        voice: String,
    },
    Exit,
}

/// The agent messages the host has sent. `assistant_message` and `turn_end` both name the
/// `session_id` they belong to, and auto-speak answers for a project, not for a session — so with
/// two sessions in one project a shared cache would read out whichever message arrived last, no
/// matter whose turn ended. `newest` is kept apart because `/tts` and the panel's `s` are the user
/// asking for what they last saw, whichever session wrote it.
#[derive(Default)]
struct Messages {
    newest: String,
    by_session: HashMap<String, String>,
}

impl Messages {
    fn record(&mut self, session: &str, text: &str) {
        self.newest = text.to_string();
        self.by_session
            .insert(session.to_string(), text.to_string());
    }

    /// What the session that just ended last said. A host that omits `session_id` (the field is
    /// optional in the protocol) puts both frames in the same `""` slot and behaves as one session.
    fn for_session(&self, session: &str) -> &str {
        self.by_session
            .get(session)
            .map(String::as_str)
            .unwrap_or("")
    }
}

/// The `session_id` an event frame belongs to, or `""` when the host does not name one.
fn frame_session(frame: &Value) -> &str {
    frame
        .get("session_id")
        .and_then(Value::as_str)
        .unwrap_or("")
}

fn main() {
    let out = Out::new();
    let messages = Arc::new(Mutex::new(Messages::default()));
    let playing = Arc::new(AtomicBool::new(false));

    let hub = cache::hf_hub();
    // Only voices this model can speak are offered or registered. The full language-filtered
    // catalog (available and not) drives the picker; registration picks out the local ones.
    let catalog = voices::catalog(&hub, tts::MODEL_LANGUAGE);
    let available: Vec<VoiceInfo> = catalog.iter().filter(|v| v.available).cloned().collect();

    let settings = Arc::new(Mutex::new(Settings::default()));

    let (tx, rx): (Sender<Cmd>, Receiver<Cmd>) = mpsc::channel();
    let mut player = {
        let out = out.clone();
        let playing = playing.clone();
        let catalog = catalog.clone();
        let current = settings.clone();
        Some(thread::spawn(move || {
            playback_thread(out, playing, current, rx, catalog)
        }))
    };

    let mut panel = SettingsPanel::new(catalog.clone());
    let mut panel_open = false;
    let mut settings_path: Option<PathBuf> = None;
    // The project the session is running in, from `hello_ack.cwd` and refreshed by every event
    // frame. Auto-speak is scoped per project, so this is what its answer is looked up against.
    let mut project = PathBuf::from(".");

    out.send(&json!({
        "type": "hello",
        "name": NAME,
        "version": VERSION,
        "capabilities": ["commands", "events", "panels"],
    }));

    let stdin = io::stdin();
    for line in stdin.lock().lines() {
        let line = match line {
            Ok(line) => line,
            Err(_) => break,
        };
        let frame: Value = match serde_json::from_str(&line) {
            Ok(frame) => frame,
            Err(_) => continue, // ignore malformed lines, like the Go version
        };
        match frame.get("type").and_then(Value::as_str).unwrap_or("") {
            "hello_ack" => {
                let dir = frame
                    .get("data_dir")
                    .and_then(Value::as_str)
                    .or_else(|| frame.get("extension_dir").and_then(Value::as_str));
                if let Some(dir) = dir {
                    settings_path = Some(PathBuf::from(dir).join("settings.json"));
                }
                // The extension process runs from its own directory, so the project path has to
                // come from the host rather than the working directory.
                if let Some(cwd) = frame.get("cwd").and_then(Value::as_str) {
                    project = PathBuf::from(cwd);
                }
                let mut loaded = settings_path
                    .as_ref()
                    .map(|path| settings::load(path))
                    .unwrap_or_default();
                // Seed a concrete voice so the picker and `/tts` show one that exists locally.
                let mut seeded = false;
                if loaded.voice.is_empty() {
                    if let Some(first) = available.first() {
                        loaded.voice = first.name.clone();
                        seeded = true;
                    }
                }
                *settings.lock().unwrap() = loaded.clone();
                // Persist the seeded defaults so a valid settings.json exists from the very first
                // run (not only after a panel edit), keeping the chosen default across restarts.
                if seeded {
                    if let Some(path) = &settings_path {
                        if let Err(e) = settings::save(path, &loaded) {
                            eprintln!("zot-speaks: could not save seeded settings: {e}");
                        }
                    }
                }

                out.send(&json!({
                    "type": "register_command",
                    "name": "tts",
                    "description": "speak the last agent message (or given text) via Phonon; `/tts stop` cuts it off",
                }));
                out.send(&json!({
                    "type": "register_command",
                    "name": "tts-settings",
                    "description": "pick voice, volume, speed, temperature and auto-speak; the voice picker previews each voice as you browse, volume and speed changes audition the current voice, the Temperature row explains itself out loud while it is selected, `f` pins favorites, and the arrows on Auto speak step it off -> this project -> every project, where speaking the last agent message at the end of a turn is what changes",
                }));
                out.send(
                    &json!({"type": "subscribe", "events": ["assistant_message", "turn_end"]}),
                );
                out.send(&json!({"type": "ready"}));
            }
            "event" => {
                // Every lifecycle notification carries the cwd it happened in; sessions can move
                // between projects, so the scoping follows the event rather than startup. An open
                // panel has to follow too, or its arrows would edit the project we opened it in
                // rather than the one being spoken for, with no way to see that.
                if let Some(cwd) = frame.get("cwd").and_then(Value::as_str) {
                    let cwd = PathBuf::from(cwd);
                    if cwd != project {
                        project = cwd;
                        if panel_open {
                            panel.set_project(project.clone());
                            out.send(&json!({
                                "type": "panel_render",
                                "panel_id": PANEL_ID,
                                "title": panel.title(),
                                "lines": panel.lines(),
                                "footer": panel.footer(),
                            }));
                        }
                    }
                }
                match frame.get("event").and_then(Value::as_str).unwrap_or("") {
                    "assistant_message" => {
                        if let Some(text) = frame.get("text").and_then(Value::as_str) {
                            messages.lock().unwrap().record(frame_session(&frame), text);
                        }
                    }
                    // `turn_end` fires after every model step, so its stop reason is what
                    // separates the answer worth reading out from a step that asked for a tool and
                    // carried on working. The host's names are its own, not the provider's: zot
                    // reports the final step as `end` (`extensions.md` still shows `end_turn`), so
                    // the gate lists the reasons that mean "not finished" and treats the rest as an
                    // answer.
                    "turn_end" => {
                        let stop = frame.get("stop").and_then(Value::as_str).unwrap_or("");
                        if is_final_stop(stop) {
                            auto_speak(&messages, &tx, &settings, &project, frame_session(&frame));
                        }
                    }
                    _ => {}
                }
            }
            "command_invoked" => {
                let id = frame.get("id").and_then(Value::as_str).unwrap_or("");
                let name = frame.get("name").and_then(Value::as_str).unwrap_or("");
                let args = frame.get("args").and_then(Value::as_str).unwrap_or("");
                match name {
                    "tts" => handle_tts(&out, &messages, &playing, &tx, &settings, id, args),
                    "tts-settings" => {
                        panel.set_project(project.clone());
                        panel.open(&settings.lock().unwrap().clone());
                        panel_open = true;
                        out.send(&json!({
                            "type": "command_response",
                            "id": id,
                            "action": "open_panel",
                            "open_panel": {
                                "id": PANEL_ID,
                                "title": panel.title(),
                                "lines": panel.lines(),
                                "footer": panel.footer(),
                            },
                        }));
                    }
                    other => out.send(&json!({
                        "type": "command_response",
                        "id": id,
                        "error": format!("unknown command '{other}'"),
                    })),
                }
            }
            "panel_key" if panel_open => {
                if frame.get("panel_id").and_then(Value::as_str) == Some(PANEL_ID) {
                    let key = frame.get("key").and_then(Value::as_str).unwrap_or("");
                    let text = frame.get("text").and_then(Value::as_str).unwrap_or("");
                    match panel.on_key(key, text) {
                        KeyOutcome::Continue { changed, preview } => {
                            if changed {
                                let snapshot = panel.settings().clone();
                                *settings.lock().unwrap() = snapshot.clone();
                                if let Some(path) = &settings_path {
                                    if let Err(e) = settings::save(path, &snapshot) {
                                        eprintln!("zot-speaks: could not save settings: {e}");
                                    }
                                }
                            }
                            out.send(&json!({
                                "type": "panel_render",
                                "panel_id": PANEL_ID,
                                "title": panel.title(),
                                "lines": panel.lines(),
                                "footer": panel.footer(),
                            }));
                            // The panel decided what the key means; here we make it audible.
                            if let Some(preview) = preview {
                                speak_preview(&messages, &tx, &settings, preview);
                            }
                        }
                        KeyOutcome::Close => {
                            panel_open = false;
                            // An audition outlives its panel unless we cut it off on the way out.
                            let _ = tx.send(Cmd::Stop);
                            out.send(&json!({"type": "panel_close", "panel_id": PANEL_ID}));
                        }
                    }
                }
            }
            "panel_close" => {
                if frame.get("panel_id").and_then(Value::as_str) == Some(PANEL_ID) {
                    panel_open = false;
                    let _ = tx.send(Cmd::Stop); // closing from the host ends the audition too
                }
            }
            "shutdown" => {
                let _ = tx.send(Cmd::Stop);
                let _ = tx.send(Cmd::Exit);
                out.send(&json!({"type": "shutdown_ack"}));
                join_player(&mut player);
                return;
            }
            _ => {}
        }
    }

    // stdin closed without an explicit shutdown: still end playback cleanly.
    let _ = tx.send(Cmd::Exit);
    join_player(&mut player);
}

fn join_player(player: &mut Option<JoinHandle<()>>) {
    if let Some(handle) = player.take() {
        // The playback thread reaps its sink child before returning, so joining here
        // guarantees no orphaned sink process on exit.
        let _ = handle.join();
    }
}

fn handle_tts(
    out: &Arc<Out>,
    messages: &Arc<Mutex<Messages>>,
    playing: &Arc<AtomicBool>,
    tx: &Sender<Cmd>,
    settings: &Arc<Mutex<Settings>>,
    id: &str,
    args: &str,
) {
    let display = |message: &str| {
        out.send(
            &json!({"type": "command_response", "id": id, "action": "display", "display": message}),
        );
    };

    if args == "stop" {
        // Report what we believed was happening; the playback thread does the cooperative stop.
        let was_playing = playing.load(Ordering::SeqCst);
        let _ = tx.send(Cmd::Stop);
        display(if was_playing {
            "zot-speaks: stopped"
        } else {
            "zot-speaks: nothing playing"
        });
        return;
    }

    let (text, src) = if !args.is_empty() {
        (args.to_string(), "text")
    } else {
        // `/tts` is the user asking for what they just read, which is the newest message in the
        // process regardless of which session wrote it.
        (messages.lock().unwrap().newest.clone(), "last message")
    };
    if text.is_empty() {
        display("zot-speaks: nothing to speak yet");
        return;
    }

    // Snapshot the current settings so a `/tts` honours the latest panel edits.
    let snapshot = settings.lock().unwrap().clone();
    out.notify("info", &format!("zot-speaks: synthesizing {src}…"));
    match tx.send(Cmd::Play {
        text,
        settings: snapshot,
    }) {
        // Reply immediately; audio is streamed on the playback thread.
        Ok(()) => out.send(&json!({"type": "command_response", "id": id, "action": "noop"})),
        Err(_) => display("zot-speaks: playback unavailable (model failed to load)"),
    }
}

/// Whether a `turn_end` stop reason means the agent finished its answer. `tool_use` means another
/// step follows; the failure reasons mean there is no finished answer to read out. Everything
/// else counts as an answer — including a stop name this extension has never seen — because
/// silently never speaking is the worse way to be wrong about a host's vocabulary.
fn is_final_stop(stop: &str) -> bool {
    !matches!(
        stop,
        "" | "tool_use" | "error" | "aborted" | "cancelled" | "refusal"
    )
}

/// Speak what the session that just finished last said, for projects that asked for it
/// (`auto_speak`, scoped per project — see `Settings::auto_speak_for`). Silent when the project has
/// it off, or when that session never produced a message: a message cached from another session is
/// not this turn's answer. The playback thread reports the speech it starts, so this adds no
/// notification of its own.
fn auto_speak(
    messages: &Arc<Mutex<Messages>>,
    tx: &Sender<Cmd>,
    settings: &Arc<Mutex<Settings>>,
    project: &Path,
    session: &str,
) {
    let snapshot = settings.lock().unwrap().clone();
    if !snapshot.auto_speak_for(project) {
        return;
    }
    let text = messages.lock().unwrap().for_session(session).to_string();
    if text.trim().is_empty() {
        return;
    }
    let _ = tx.send(Cmd::Play {
        text,
        settings: snapshot,
    });
}

/// Make a panel key press audible: a voice introducing itself, the last agent message in the
/// highlighted voice, a line the panel wants repeated, or silence (the cursor landed on a voice whose
/// embedding is not cached, so the audition we are leaving must not keep talking). Previews share the
/// playback queue with `/tts`, which is what makes a new one cut off the previous one rather than
/// queue behind it.
fn speak_preview(
    messages: &Arc<Mutex<Messages>>,
    tx: &Sender<Cmd>,
    settings: &Arc<Mutex<Settings>>,
    preview: Preview,
) {
    let base = settings.lock().unwrap().clone();
    let (text, voice) = match preview {
        Preview::Voice { voice, text } => (text, voice),
        Preview::LastMessage { voice } => (messages.lock().unwrap().newest.clone(), voice),
        Preview::Explain { voice, text } => {
            // Only the voice travels with a repeated line: the thread reads the rest of the settings
            // again before each repetition, which is how a temperature you just stepped is heard in
            // the next sentence.
            let _ = tx.send(Cmd::Loop { text, voice });
            return;
        }
        Preview::Silence => {
            let _ = tx.send(Cmd::Stop);
            return;
        }
    };
    if text.trim().is_empty() {
        // Nothing to audition: at least leave the panel quiet.
        let _ = tx.send(Cmd::Stop);
        return;
    }
    // An audition changes only the voice; volume and temperature stay as the settings have them.
    let request = Settings { voice, ..base };
    let _ = tx.send(Cmd::Play {
        text,
        settings: request,
    });
}

/// Own the `Synth` on a dedicated thread: load it once (in the background, while the protocol is
/// already live), then serve Play/Loop/Stop/Exit commands. A request enqueued during warmup sits in
/// the channel until we start reading.
fn playback_thread(
    out: Arc<Out>,
    playing: Arc<AtomicBool>,
    current: Arc<Mutex<Settings>>,
    rx: Receiver<Cmd>,
    catalog: Vec<VoiceInfo>,
) {
    out.notify("info", "zot-speaks: loading Phonon model…");
    let synth = match tts::load_synth(&catalog) {
        Ok(synth) => synth,
        Err(e) => {
            out.notify("error", &format!("zot-speaks: model load failed: {e}"));
            return; // /tts will report playback unavailable
        }
    };
    out.notify(
        "info",
        &format!("zot-speaks: ready ({} Hz).", synth.sample_rate()),
    );

    // One noise seed per utterance, drawn as the utterance starts. With ptts' own default the same
    // sentence would come back as the identical take every time it is spoken.
    let seeds = tts::SeedPlan::from_env();

    loop {
        let request = match rx.recv() {
            Ok(Cmd::Play { text, settings }) => Resume::Play { text, settings },
            Ok(Cmd::Loop { text, voice }) => Resume::Loop { text, voice },
            Ok(Cmd::Stop) => continue, // idle: nothing to stop
            Ok(Cmd::Exit) => return,
            Err(_) => return, // all senders dropped
        };
        if !serve(&synth, &seeds, &out, &playing, &current, &rx, request) {
            return;
        }
    }
}

/// Play what was asked for, and then whatever the queue asks for, until something says otherwise.
/// A `Resume::Loop` is what makes this a repeating utterance: when a repetition ends with nothing
/// else queued, the same line is spoken again from the live settings, so the temperature on screen is
/// the temperature you hear in the next sentence. Anything else is one utterance. The coalescing that
/// used to live here is now `coalesce`, applied before every utterance rather than only around the
/// ones a held key produced.
///
/// Returns false when the thread should exit.
fn serve(
    synth: &Synth,
    seeds: &tts::SeedPlan,
    out: &Arc<Out>,
    playing: &Arc<AtomicBool>,
    current: &Arc<Mutex<Settings>>,
    rx: &Receiver<Cmd>,
    request: Resume,
) -> bool {
    let mut next = request;
    loop {
        let (text, settings, repeat) = match coalesce(rx, next) {
            Resume::Play { text, settings } => (text, settings, false),
            Resume::Loop { text, voice } => (text, live_settings(current, &voice), true),
            Resume::Idle | Resume::Finished => return true, // nothing left to say
            Resume::Exit => return false,
        };
        match play_one(synth, seeds, out, playing, rx, &text, &settings) {
            // Stopped, or nothing playable: go quiet.
            Resume::Idle => return true,
            Resume::Exit => return false,
            // The line said its piece and nothing else is queued. That is the one moment a repeat
            // repeats, and a breath comes first so it lands as a sentence and not a stutter.
            Resume::Finished if repeat => {
                thread::sleep(LOOP_GAP);
                next = Resume::Loop {
                    text,
                    voice: settings.voice.clone(),
                };
            }
            Resume::Finished => return true,
            // Cut short by a newer request, which decides what comes next, repeating included.
            Resume::Play { text, settings } => next = Resume::Play { text, settings },
            Resume::Loop { text, voice } => next = Resume::Loop { text, voice },
        }
    }
}

/// A request superseded mid-stream is usually a held arrow key in the picker: one preview per step,
/// all of them now pointless. Drain the queue and keep only the newest request, so the voice under
/// the cursor is the one you hear instead of a backlog of voices you walked past. A drain that found
/// nothing hands back the request it was given, which is what makes a repeat keep repeating.
fn coalesce(rx: &Receiver<Cmd>, request: Resume) -> Resume {
    let mut latest = request;
    loop {
        match rx.try_recv() {
            Ok(Cmd::Play { text, settings }) => latest = Resume::Play { text, settings },
            Ok(Cmd::Loop { text, voice }) => latest = Resume::Loop { text, voice },
            Ok(Cmd::Stop) => latest = Resume::Idle,
            Ok(Cmd::Exit) => return Resume::Exit,
            Err(TryRecvError::Empty) => return latest,
            Err(TryRecvError::Disconnected) => return Resume::Exit,
        }
    }
}

/// The settings a repetition is spoken at: the shared live settings — so the value on screen is the
/// value you hear — carried in the voice the explanation was started in, which is the voice the user
/// is listening to.
fn live_settings(current: &Arc<Mutex<Settings>>, voice: &str) -> Settings {
    Settings {
        voice: voice.to_string(),
        ..current.lock().unwrap().clone()
    }
}

/// Synthesize + stream one text under the given settings, watching for a cooperative Stop or a
/// superseding Play between PCM chunks. `seeds` is asked for one seed per utterance, which is what
/// makes a repeated line a new take rather than the same one replayed. Returns what the caller
/// should do next.
fn play_one(
    synth: &Synth,
    seeds: &tts::SeedPlan,
    out: &Arc<Out>,
    playing: &AtomicBool,
    rx: &Receiver<Cmd>,
    text: &str,
    settings: &Settings,
) -> Resume {
    let (options, used_voice) = tts::build_options(synth, settings, seeds.next());
    if !settings.voice.is_empty() && used_voice.is_none() {
        out.notify(
            "warn",
            &format!(
                "zot-speaks: voice '{}' is not registered locally; using default",
                settings.voice
            ),
        );
    }
    let mut stream = match synth.stream_with(text, &options) {
        Ok(stream) => stream,
        Err(e) => {
            out.notify("error", &format!("zot-speaks: synthesis failed: {e}"));
            return Resume::Idle;
        }
    };
    let rate = stream.sample_rate();
    // Start the player. Spawn is part of streaming, not a preflight: the pipe exists while the model
    // is still working, so sound starts at the first chunk. A player that starts and then dies is not
    // a spawn failure — it is reported where it actually shows up, at the teardown.
    let speed = settings.speed;
    let mut sink = match Sink::open(rate, speed, settings.gain()) {
        Ok(sink) => sink,
        Err(e) => {
            // "Not installed", "present but not startable" and "no stdin" are three different
            // sentences and Sink names them; only the first is an install hint.
            out.notify("error", &format!("zot-speaks: {}", e.report()));
            return Resume::Idle;
        }
    };

    // Announced only now that the player is actually running: a request that could not be played is
    // not announced as if it were. The player is not named — there is one, and it earns a mention
    // only when it fails.
    out.notify(
        "info",
        &format!(
            "zot-speaks: speaking as {} at temp {:.2}, vol {:.0}%, speed {}",
            used_voice.clone().unwrap_or_else(|| "default".to_string()),
            options.temperature.unwrap_or(0.0),
            settings.volume * 100.0,
            speed.label(),
        ),
    );

    playing.store(true, Ordering::SeqCst);
    // An utterance that reaches the end of its stream has finished; that is what a repeat request
    // waits for. A Stop, a player that gave up, or a line with no audio in it says `Idle` instead.
    let mut resume = Resume::Finished;
    // Audio the user has moved on from: drives the bounded grace at the teardown below.
    let mut cut_short = false;

    for chunk in stream.by_ref() {
        match chunk {
            Ok(samples) => {
                // `write` is the whole feed: encode, hand over, and wait out the share of real time
                // the chunk represents, so the sink buffer stays small. That pacing is what makes
                // both TTFB low and the stop tail short.
                if sink.write(&samples).is_err() {
                    break; // sink vanished (the player died); stop cleanly
                }
            }
            Err(e) => {
                out.notify("error", &format!("zot-speaks: stream error: {e}"));
                break;
            }
        }

        // Cooperative control point: between chunks, honor Stop / new Play / new Loop / Exit.
        match rx.try_recv() {
            Ok(Cmd::Stop) => {
                resume = Resume::Idle;
                cut_short = true;
                break;
            }
            Ok(Cmd::Play { text, settings }) => {
                resume = Resume::Play { text, settings };
                cut_short = true;
                break;
            }
            Ok(Cmd::Loop { text, voice }) => {
                // The panel wants this line said on repeat. Cutting this utterance short is right
                // even when it is that very line playing: the panel only asks for a repeat when it
                // has stopped asking, so what follows is the same sentence at a new value.
                resume = Resume::Loop { text, voice };
                cut_short = true;
                break;
            }
            Ok(Cmd::Exit) => {
                resume = Resume::Exit;
                break;
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                resume = Resume::Exit;
                break;
            }
        }
    }

    playing.store(false, Ordering::SeqCst);
    drop(stream); // SpeechStream::drop halts generation workers.

    // The teardown is the sink's: close the pipe, wait for the player or kill it, and find out what
    // it has to say about how its run ended. A player that gave up is not worth repeating after.
    let finished = sink.finish(cut_short);
    if let Some(reason) = finished.failure() {
        out.notify("error", &format!("zot-speaks: {reason}"));
        resume = Resume::Idle;
    }
    // A line that produced no audio at all is nothing to repeat either; otherwise a broken player
    // would make an explanation retry forever.
    if finished.silent() {
        resume = Resume::Idle;
    }
    resume
}

#[cfg(test)]
mod tests {
    use super::{coalesce, is_final_stop, live_settings, Cmd, Messages, Resume};
    use std::sync::mpsc::{self, Sender};
    use std::sync::{Arc, Mutex};
    use zot_speaks::settings::Settings;

    /// Auto-speak hangs on this gate, so it is worth naming the values out loud. The host's own
    /// name for a finished answer (`end`, what zot puts on the wire) and the provider's name the
    /// docs show (`end_turn`) both have to count; a step that goes on working does not.
    #[test]
    fn only_a_finished_answer_counts_as_one() {
        for stop in ["end", "end_turn", "length", "max_tokens", "some_new_name"] {
            assert!(is_final_stop(stop), "{stop} should read as an answer");
        }
        for stop in ["", "tool_use", "error", "aborted", "cancelled", "refusal"] {
            assert!(!is_final_stop(stop), "{stop} should not speak");
        }
    }

    /// One process serves every session of a project, so the cache has to know whose message is
    /// whose: a `turn_end` may only read out what *that* session last said.
    #[test]
    fn a_session_only_ever_hears_its_own_message() {
        let mut m = Messages::default();
        m.record("sess-a", "written while the user was elsewhere");
        m.record("sess-b", "this turn's answer");
        assert_eq!(
            m.for_session("sess-a"),
            "written while the user was elsewhere"
        );
        assert_eq!(m.for_session("sess-b"), "this turn's answer");
        // A session that never produced a message has nothing to read out, even though another
        // session's message sits in the same cache. This is the bug: speaking it would pass.
        assert_eq!(m.for_session("sess-c"), "");
    }

    /// A session's slot holds its latest message, and `/tts` reads the newest one seen whatever
    /// wrote it.
    #[test]
    fn a_session_keeps_its_latest_and_the_process_keeps_the_newest() {
        let mut m = Messages::default();
        m.record("sess-a", "first");
        m.record("sess-a", "second");
        m.record("sess-b", "third");
        assert_eq!(
            m.for_session("sess-a"),
            "second",
            "a slot holds its latest message"
        );
        assert_eq!(m.newest, "third", "/tts reads what was last written");
    }

    /// A repeat repeats because a drain that found nothing hands back the request it was given: the
    /// queue is the only thing that ends it. So the queue's three ways of ending one have to win in
    /// the right order — a Stop, a newer request, an Exit.
    #[test]
    fn coalescing_repeats_until_the_queue_says_otherwise() {
        let repeat = || Resume::Loop {
            text: "one line".to_string(),
            voice: "Miles".to_string(),
        };
        let (tx, rx) = mpsc::channel();
        assert!(
            matches!(coalesce(&rx, repeat()), Resume::Loop { .. }),
            "an empty drain must hand the repeat back, not drop it"
        );
        tx.send(Cmd::Stop).unwrap();
        assert!(
            matches!(coalesce(&rx, repeat()), Resume::Idle),
            "a Stop ends a repeat"
        );
        tx.send(Cmd::Loop {
            text: "another line".to_string(),
            voice: "Zoey".to_string(),
        })
        .unwrap();
        match coalesce(&rx, repeat()) {
            Resume::Loop { text, voice } => {
                assert_eq!((text.as_str(), voice.as_str()), ("another line", "Zoey"))
            }
            _ => panic!("the newest request should win over the one it supersedes"),
        }
        tx.send(Cmd::Exit).unwrap();
        assert!(
            matches!(coalesce(&rx, repeat()), Resume::Exit),
            "exit outranks a repeat"
        );
        // The drain itself is what the thread blocks on between utterances, so a sender that is
        // still alive must never look like an ended conversation.
        let held: Sender<Cmd> = tx.clone();
        assert!(matches!(coalesce(&rx, repeat()), Resume::Loop { .. }));
        drop(held);
        drop(tx);
        assert!(
            matches!(coalesce(&rx, repeat()), Resume::Exit),
            "a closed queue is an exit"
        );
    }

    /// Every repetition is spoken at the settings the panel is showing, but in the voice the
    /// explanation started in: the row the user is on is the value they are hearing, and the voice
    /// they picked is the one they are listening to.
    #[test]
    fn a_repetition_follows_the_live_settings_but_keeps_its_voice() {
        let live = Arc::new(Mutex::new(Settings {
            temperature: 0.70,
            volume: 0.40,
            ..Settings::default()
        }));
        let spoken = live_settings(&live, "Zoey");
        assert_eq!(spoken.voice, "Zoey", "a repeat keeps its voice");
        assert!(
            (spoken.temperature - 0.70).abs() < 1e-6,
            "a repeat hears the temperature on screen"
        );
        assert!(
            (spoken.volume - 0.40).abs() < 1e-6,
            "a repeat follows the dial as it moves"
        );
    }
}
