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
//!   shutdown -> stop, shutdown_ack, exit
//!
//! Playback design (unchanged from the validated slice):
//!   * `stream_with` yields mono f32 PCM chunks, so time-to-first-audio is decoupled from length.
//!   * Each chunk is scaled by the volume dial (`Settings::gain`), converted to s16le, and piped to
//!     a `paplay` sink.
//!   * Voice and temperature go through `SpeechOptions`, resolved against the locally-registered
//!     voice catalog. Stop is cooperative: we drop the `SpeechStream` and close `paplay`'s stdin.
//!   * Writes are paced to real time so `paplay` never buffers far ahead — low TTFB, short stop tail.

mod cache;
mod panel;
mod settings;
mod tts;
mod voices;
mod wire;

use std::collections::HashMap;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use ptts::synth::Synth;
use serde_json::{json, Value};

use panel::{KeyOutcome, Preview, SettingsPanel, PANEL_ID};
use settings::Settings;
use voices::VoiceInfo;
use wire::Out;

const NAME: &str = "zot-speaks";
/// One version per artifact: the package version in `Cargo.toml`, which `dev/test.sh` checks
/// against the version in `extension.json` before it accepts the hello frame.
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// How far ahead of real-time playback we run: small enough that a stop goes quiet within a
/// chunk, large enough that `paplay` never starves.
const PLAY_LEAD: Duration = Duration::from_millis(40);

enum Cmd {
    Play { text: String, settings: Settings },
    Stop,
    Exit,
}

enum Resume {
    Idle,
    Play { text: String, settings: Settings },
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
    // Give `ort` a sane thread count once, before any thread touches env or the model.
    apply_ptts_env();

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
        Some(thread::spawn(move || {
            playback_thread(out, playing, rx, catalog)
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
                    "description": "pick voice, volume, temperature and auto-speak; the voice picker previews each voice as you browse, volume changes audition the current voice, `f` pins favorites, and the arrows on Auto speak step it off -> this project -> every project, where speaking the last agent message at the end of a turn is what changes",
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
        // The playback thread reaps its `paplay` child before returning, so joining here
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
/// highlighted voice, or silence (the cursor landed on a voice whose embedding is not cached, so
/// the audition we are leaving must not keep talking). Previews share the playback queue with
/// `/tts`, which is what makes a new one cut off the previous one rather than queue behind it.
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
/// already live), then serve Play/Stop/Exit commands. A Play enqueued during warmup sits in the
/// channel until we start reading.
fn playback_thread(
    out: Arc<Out>,
    playing: Arc<AtomicBool>,
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

    loop {
        match rx.recv() {
            Ok(Cmd::Play { text, settings }) => {
                // Coalesce before synthesizing as well as after: while we are busy the user may
                // already have moved on, and the voices they walked past are not worth hearing.
                let mut pending = match latest_of(&rx, text, settings) {
                    Resume::Play { text, settings } => Some((text, settings)),
                    Resume::Idle => None, // a Stop had the last word
                    Resume::Exit => return,
                };
                while let Some((text, settings)) = pending.take() {
                    match play_one(&synth, &out, &playing, &rx, &text, &settings) {
                        Resume::Play { text, settings } => match latest_of(&rx, text, settings) {
                            Resume::Play { text, settings } => pending = Some((text, settings)),
                            Resume::Idle => break, // a Stop was the last word: go quiet
                            Resume::Exit => return,
                        },
                        Resume::Exit => return,
                        Resume::Idle => break,
                    }
                }
            }
            Ok(Cmd::Stop) => {} // idle: nothing to stop
            Ok(Cmd::Exit) => return,
            Err(_) => return, // all senders dropped
        }
    }
}

/// A request superseded mid-stream is usually a held arrow key in the picker: one preview per
/// step, all of them now pointless. Drain the queue and keep only the newest Play, so the voice
/// under the cursor is the one you hear instead of a backlog of voices you walked past.
fn latest_of(rx: &Receiver<Cmd>, text: String, settings: Settings) -> Resume {
    let mut latest = Resume::Play { text, settings };
    loop {
        match rx.try_recv() {
            Ok(Cmd::Play { text, settings }) => latest = Resume::Play { text, settings },
            Ok(Cmd::Stop) => latest = Resume::Idle,
            Ok(Cmd::Exit) => return Resume::Exit,
            Err(TryRecvError::Empty) => return latest,
            Err(TryRecvError::Disconnected) => return Resume::Exit,
        }
    }
}

/// Synthesize + stream one text under the given settings, watching for a cooperative Stop or a
/// superseding Play between PCM chunks. Returns what the caller should do next.
fn play_one(
    synth: &Synth,
    out: &Arc<Out>,
    playing: &AtomicBool,
    rx: &Receiver<Cmd>,
    text: &str,
    settings: &Settings,
) -> Resume {
    let (options, used_voice) = tts::build_options(synth, settings);
    if !settings.voice.is_empty() && used_voice.is_none() {
        out.notify(
            "warn",
            &format!(
                "zot-speaks: voice '{}' is not registered locally; using default",
                settings.voice
            ),
        );
    }
    out.notify(
        "info",
        &format!(
            "zot-speaks: speaking as {} at temp {:.2}, vol {:.0}%",
            used_voice.clone().unwrap_or_else(|| "default".to_string()),
            options.temperature.unwrap_or(0.0),
            settings.volume * 100.0,
        ),
    );

    let mut stream = match synth.stream_with(text, &options) {
        Ok(stream) => stream,
        Err(e) => {
            out.notify("error", &format!("zot-speaks: synthesis failed: {e}"));
            return Resume::Idle;
        }
    };
    let rate = stream.sample_rate();

    let rate_arg = rate.to_string();
    let mut child = match Command::new("paplay")
        .args([
            "--raw",
            "--format=s16le",
            "--channels=1",
            "--rate",
            &rate_arg,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(e) => {
            out.notify(
                "error",
                &format!("zot-speaks: failed to start `paplay`: {e}"),
            );
            return Resume::Idle;
        }
    };
    let mut sink = match child.stdin.take() {
        Some(sink) => sink,
        None => {
            out.notify("error", "zot-speaks: paplay stdin unavailable");
            return Resume::Idle;
        }
    };

    let gain = settings.gain();
    playing.store(true, Ordering::SeqCst);
    let started = Instant::now();
    let mut samples_written: u64 = 0;
    let mut resume = Resume::Idle;
    let mut buffer: Vec<u8> = Vec::new();

    for chunk in stream.by_ref() {
        match chunk {
            Ok(samples) => {
                buffer.clear();
                buffer.reserve(samples.len() * 2);
                for &sample in &samples {
                    let value = if sample.is_finite() {
                        (sample * gain).clamp(-1.0, 1.0)
                    } else {
                        0.0
                    };
                    buffer.extend_from_slice(
                        &((value * 32767.0).round().clamp(-32768.0, 32767.0) as i16).to_le_bytes(),
                    );
                }
                if sink.write_all(&buffer).is_err() {
                    break; // sink vanished (paplay died); stop cleanly
                }
                samples_written += samples.len() as u64;

                // Pace to real time so the sink buffer stays small: this is what makes both
                // TTFB low and the stop tail short.
                let audio = Duration::from_secs_f64(samples_written as f64 / rate as f64);
                if audio > started.elapsed() + PLAY_LEAD {
                    thread::sleep(audio - started.elapsed() - PLAY_LEAD);
                }
            }
            Err(e) => {
                out.notify("error", &format!("zot-speaks: stream error: {e}"));
                break;
            }
        }

        // Cooperative control point: between chunks, honor Stop / new Play / Exit.
        match rx.try_recv() {
            Ok(Cmd::Stop) => {
                resume = Resume::Idle;
                break;
            }
            Ok(Cmd::Play { text, settings }) => {
                resume = Resume::Play { text, settings };
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
    drop(sink); // close paplay stdin: EOF -> paplay drains its buffer and exits.
    let _ = child.wait(); // reap the sink; no signals sent anywhere.
    resume
}

/// Give `ort` a sane thread count. Must run before the model load.
fn apply_ptts_env() {
    if let Ok(threads) = std::env::var("ZOT_TTS_THREADS") {
        std::env::set_var("OMP_NUM_THREADS", threads);
    } else {
        let available = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4);
        std::env::set_var("OMP_NUM_THREADS", available.max(1).to_string());
    }
    std::env::set_var("ORT_DISABLE_THREAD_POOLS", "1");
    std::env::set_var("ORT_SESSION_INTRA_OP_THREAD_POOL_SIZE", "0");
}

#[cfg(test)]
mod tests {
    use super::{is_final_stop, Messages};

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
}
