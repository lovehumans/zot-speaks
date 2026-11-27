//! zot-speaks — speak the last agent message aloud via Gradium Phonon.
//!
//! Protocol (JSON-lines on stdin/stdout), matching zot's extension docs:
//!   hello (extension-initiated) -> hello_ack -> register_command tts + tts-settings
//!     + subscribe assistant_message + turn_end + user_prompt_submit + ready
//!   assistant_message events -> cache that session's latest `text`
//!   turn_end with a final stop reason -> speak the message of the session that ended, in
//!     projects with auto-speak on
//!   user_prompt_submit -> cut off whatever is still playing, without a note: the user has sent a
//!     message, so the talk that answers the one they replaced has nothing left to say
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
//!     filter rather than re-timed here); `Sink::write` converts a chunk to s16le and feeds it at the
//!     player's pace; `Sink::finish` drains or kills the player and turns what it said into a
//!     sentence. The level is applied before the sink, in the synthesis chain (`tts::post_chain`), so
//!     the writer is a conversion and not a volume control. What is left here owns the
//!     utterance: what to synthesize, and when a Stop or a superseding request ends it.
//!   * Voice and temperature go through `SpeechOptions`, resolved against the locally-registered
//!     voice catalog, and every utterance is synthesized from its own noise seed (`tts::SeedPlan`;
//!     `ZOT_SPEAKS_SEED` pins the sequence) so a repeated line is a new take, not the same one.
//!     Stop is cooperative: we drop the `SpeechStream` and close the sink's stdin.
//!   * Pacing to real time lives in the sink, so the player never buffers far ahead — low TTFB,
//!     short tail.
//!   * What reaches the model is filtered first (`tts::spoken_text`): fenced code, markdown table
//!     rows and every inline code span are dropped, the last wherever they sit — alone or inside a pair
//!     of parentheses — so nothing that only appears in code is ever pronounced (ADR-0039), and a heading
//!     keeps its words with a full stop added after them. The filter runs in
//!     `zot_speaks::queue`, on the way to `play_one`, which every utterance passes through — an
//!     auto-spoken answer, a `/tts`, a panel audition.

mod panel;
mod wire;

use std::collections::HashMap;
use std::io::{self, BufRead};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use ptts::synth::Synth;
use serde_json::{json, Value};

use zot_speaks::cache;
use zot_speaks::queue::{self, Outcome, Request, Utterance};
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

    let (tx, rx): (Sender<Request>, Receiver<Request>) = mpsc::channel();
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
                out.send(&json!({
                    "type": "subscribe",
                    "events": ["assistant_message", "turn_end", "user_prompt_submit"],
                }));
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
                            render_panel(&out, &panel);
                        }
                    }
                }
                match frame.get("event").and_then(Value::as_str).unwrap_or("") {
                    // The host fires this the moment it accepts what the user typed, before the
                    // input is appended or queued. An utterance still playing then answers a message
                    // the user has already moved past, so it is cut off exactly the way `/tts stop`
                    // is — and silently, because typing the message is the reason and needs no note
                    // about it. This covers every kind of talk: an auto-spoken answer, a `/tts`,
                    // a panel audition.
                    "user_prompt_submit" => {
                        let _ = tx.send(Request::Stop);
                    }
                    "assistant_message" => {
                        if let Some(text) = frame.get("text").and_then(Value::as_str) {
                            messages.lock().unwrap().record(frame_session(&frame), text);
                        }
                    }
                    // `turn_end` fires after every model step, so its stop reason is what
                    // separates the answer worth reading out from a step that asked for a tool and
                    // carried on working. The host's names are its own, not the provider's: zot
                    // reports the final step as `end` (the installed host docs still show `end_turn`), so
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
                            render_panel(&out, &panel);
                            // The panel decided what the key means; here we make it audible.
                            if let Some(preview) = preview {
                                speak_preview(&messages, &tx, &settings, preview);
                            }
                        }
                        KeyOutcome::Close => {
                            panel_open = false;
                            // An audition outlives its panel unless we cut it off on the way out.
                            let _ = tx.send(Request::Stop);
                            out.send(&json!({"type": "panel_close", "panel_id": PANEL_ID}));
                        }
                    }
                }
            }
            "panel_close" => {
                if frame.get("panel_id").and_then(Value::as_str) == Some(PANEL_ID) {
                    panel_open = false;
                    let _ = tx.send(Request::Stop); // closing from the host ends the audition too
                }
            }
            "shutdown" => {
                let _ = tx.send(Request::Stop);
                let _ = tx.send(Request::Exit);
                out.send(&json!({"type": "shutdown_ack"}));
                join_player(&mut player);
                return;
            }
            _ => {}
        }
    }

    // stdin closed without an explicit shutdown: still end playback cleanly.
    let _ = tx.send(Request::Exit);
    join_player(&mut player);
}

fn join_player(player: &mut Option<JoinHandle<()>>) {
    if let Some(handle) = player.take() {
        // The playback thread reaps its sink child before returning, so joining here
        // guarantees no orphaned sink process on exit.
        let _ = handle.join();
    }
}

/// The repaint frame of an open panel, spelled once: the host redraws it from its id, its title,
/// its rows in order, and its footer. Opening a panel keeps its own spelling, because that frame
/// is a command response carrying a request id, and `json!` cannot splat fields into a frame
/// (measured in serde_json 1.0.154, which has no merge form).
fn render_panel(out: &Arc<Out>, panel: &SettingsPanel) {
    out.send(&json!({
        "type": "panel_render",
        "panel_id": PANEL_ID,
        "title": panel.title(),
        "lines": panel.lines(),
        "footer": panel.footer(),
    }));
}

fn handle_tts(
    out: &Arc<Out>,
    messages: &Arc<Mutex<Messages>>,
    playing: &Arc<AtomicBool>,
    tx: &Sender<Request>,
    settings: &Arc<Mutex<Settings>>,
    id: &str,
    args: &str,
) {
    // Every message below is a note: the host shows it in the notes area, where it already carries
    // this extension's name, so a message never names the extension itself (ADR-0049).
    let display = |message: &str| {
        out.send(
            &json!({"type": "command_response", "id": id, "action": "display", "display": message}),
        );
    };

    if args == "stop" {
        // Report what we believed was happening; the playback thread does the cooperative stop.
        let was_playing = playing.load(Ordering::SeqCst);
        let _ = tx.send(Request::Stop);
        display(if was_playing {
            "stopped"
        } else {
            "nothing playing"
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
        display("nothing to speak yet");
        return;
    }

    // Snapshot the current settings so a `/tts` honours the latest panel edits.
    let snapshot = settings.lock().unwrap().clone();
    out.notify("info", &format!("synthesizing {src}…"));
    match tx.send(Request::Play {
        text,
        settings: snapshot,
    }) {
        // Reply immediately; audio is streamed on the playback thread.
        Ok(()) => out.send(&json!({"type": "command_response", "id": id, "action": "noop"})),
        Err(_) => display("playback unavailable (model failed to load)"),
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
    tx: &Sender<Request>,
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
    let _ = tx.send(Request::Play {
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
    tx: &Sender<Request>,
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
            let _ = tx.send(Request::Loop { text, voice });
            return;
        }
        Preview::Silence => {
            let _ = tx.send(Request::Stop);
            return;
        }
    };
    if text.trim().is_empty() {
        // Nothing to audition: at least leave the panel quiet.
        let _ = tx.send(Request::Stop);
        return;
    }
    // An audition changes only the voice; volume and temperature stay as the settings have them.
    let request = Settings { voice, ..base };
    let _ = tx.send(Request::Play {
        text,
        settings: request,
    });
}

/// Own the `Synth` on a dedicated thread: load it once (in the background, while the protocol is
/// already live), then speak what the queue asks for. A request enqueued during warmup sits in the
/// channel until the loop starts reading it.
fn playback_thread(
    out: Arc<Out>,
    playing: Arc<AtomicBool>,
    current: Arc<Mutex<Settings>>,
    rx: Receiver<Request>,
    catalog: Vec<VoiceInfo>,
) {
    out.notify("info", "loading Phonon model…");
    let synth = match tts::load_synth(&catalog) {
        Ok(synth) => synth,
        Err(e) => {
            out.notify("error", &format!("model load failed: {e}"));
            return; // /tts will report playback unavailable
        }
    };
    out.notify("info", &format!("ready ({} Hz).", synth.sample_rate()));

    // One noise seed per utterance, drawn as the utterance starts. With ptts' own default the same
    // sentence would come back as the identical take every time it is spoken.
    let seeds = tts::SeedPlan::from_env();

    // What to speak next is the library's decision (ADR-0047). What speaking one utterance means is
    // ours, and `play_one` reads the same queue the loop drains.
    queue::speak_queue(&rx, &current, &mut |utterance| {
        play_one(&synth, &seeds, &out, &playing, &rx, utterance)
    });
}

/// Synthesize + stream one utterance, watching for a cooperative Stop or a superseding request
/// between PCM chunks. `seeds` is asked for one seed per utterance, which is what makes a repeated
/// line a new take rather than the same one replayed. The text arrives already filtered, by the queue
/// that decided this line was worth speaking (`queue::utterance_for`), and a request this control
/// point takes out of the queue is reported back as `Outcome::Next`: the loop is what speaks it next.
fn play_one(
    synth: &Synth,
    seeds: &tts::SeedPlan,
    out: &Arc<Out>,
    playing: &AtomicBool,
    rx: &Receiver<Request>,
    utterance: &Utterance,
) -> Outcome {
    let (spoken, settings) = match utterance {
        Utterance::Play { spoken, settings } | Utterance::Loop { spoken, settings } => {
            (spoken, settings)
        }
    };
    let (options, used_voice) = tts::build_options(synth, settings, seeds.next());
    if !settings.voice.is_empty() && used_voice.is_none() {
        out.notify(
            "warn",
            &format!(
                "voice '{}' is not registered locally; using default",
                settings.voice
            ),
        );
    }
    let mut stream = match synth.stream_with(spoken, &options) {
        Ok(stream) => stream,
        Err(e) => {
            out.notify("error", &format!("synthesis failed: {e}"));
            return Outcome::Quiet;
        }
    };
    let rate = stream.sample_rate();
    // Start the player. Spawn is part of streaming, not a preflight: the pipe exists while the model
    // is still working, so sound starts at the first chunk. A player that starts and then dies is not
    // a spawn failure — it is reported where it actually shows up, at the teardown.
    let speed = settings.speed;
    let mut sink = match Sink::open(rate, speed) {
        Ok(sink) => sink,
        Err(e) => {
            // "Not installed", "present but not startable" and "no stdin" are three different
            // sentences and Sink names them; only the first is an install hint.
            out.notify("error", &e.report());
            return Outcome::Quiet;
        }
    };

    // Announced only now that the player is actually running: a request that could not be played is
    // not announced as if it were. The player is not named — there is one, and it earns a mention
    // only when it fails.
    out.notify(
        "info",
        &format!(
            "speaking as {} at temp {:.2}, vol {:.0}%, speed {}",
            used_voice.clone().unwrap_or_else(|| "default".to_string()),
            options.temperature.unwrap_or(0.0),
            settings.volume * 100.0,
            speed.label(),
        ),
    );

    playing.store(true, Ordering::SeqCst);
    // An utterance that reaches the end of its stream has finished; that is what a repeat waits for.
    // A Stop, a player that gave up, or a line with no audio in it reports `Quiet` instead.
    let mut outcome = Outcome::Finished;
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
                out.notify("error", &format!("stream error: {e}"));
                break;
            }
        }

        // Cooperative control point: between chunks, honor Stop / new Play / new Loop / Exit.
        match rx.try_recv() {
            Ok(Request::Stop) => {
                outcome = Outcome::Quiet;
                cut_short = true;
                break;
            }
            Ok(request @ (Request::Play { .. } | Request::Loop { .. })) => {
                // The panel wants this line said on repeat. Cutting this utterance short is right
                // even when it is that very line playing: the panel only asks for a repeat when it
                // has stopped asking, so what follows is the same sentence at a new value. The
                // request is reported back, because the loop is what speaks it next.
                outcome = Outcome::Next(request);
                cut_short = true;
                break;
            }
            Ok(Request::Exit) => {
                outcome = Outcome::Next(Request::Exit);
                break;
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                outcome = Outcome::Next(Request::Exit);
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
        out.notify("error", reason);
    }
    // A player that gave up is not worth repeating after, and neither is a line that produced no
    // audio at all; otherwise a broken player would make an explanation retry forever. An `Exit` the
    // control point took is the one report this teardown may not overrule, because the loop has to
    // hear about it before it goes back to waiting for a request that will never arrive.
    let exiting = matches!(outcome, Outcome::Next(Request::Exit));
    if !exiting && (finished.failure().is_some() || finished.silent()) {
        outcome = Outcome::Quiet;
    }
    outcome
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
