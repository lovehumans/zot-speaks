//! The utterance queue and the decisions taken on it: which request is spoken next, whether a line
//! is worth speaking at all, and when a repetition breathes before it says itself again.
//!
//! The loop that answers "what to speak next" lives here; the utterance pipeline does not. The binary
//! that drives this loop owns the model, the player and every report it makes to the host, and hands
//! the loop one piece of code that speaks a single utterance and reports how it ended (ADR-0047).

use std::sync::mpsc::{Receiver, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::settings::Settings;
use crate::tts;

/// The breath between two repetitions of the same line, so a repeated explanation is a sentence you
/// can follow rather than a stutter. Synthesis time adds to it; this is the floor.
const LOOP_GAP: Duration = Duration::from_millis(400);

/// One request to the speaking loop.
pub enum Request {
    /// Speak `text` once, with this exact settings snapshot.
    Play {
        text: String,
        settings: Settings,
    },
    /// Speak `text` in `voice` and go on speaking it. The loop re-reads the live settings before
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

/// One utterance the loop has decided to say: text a voice can read, and the settings it is spoken
/// at. Which variant it is says whether the loop intends to ask for it again.
pub enum Utterance {
    Play { spoken: String, settings: Settings },
    Loop { spoken: String, settings: Settings },
}

/// How one utterance ended, as the pipeline reports it back to the loop.
pub enum Outcome {
    /// The line ran to its end. The only state a repeat continues from.
    Finished,
    /// Nothing to speak and nothing queued for it: a Stop had the last word, no audio came out of the
    /// line, the player gave up, or synthesis failed. A repeat ends here, which is what stops a
    /// broken player from making an explanation retry forever.
    Quiet,
    /// The pipeline stopped for this request it took out of the queue mid-utterance: a newer one for
    /// the loop to speak next, or the `Exit` that ends the loop. It is reported rather than left on
    /// the queue, because a request already taken off it cannot be put back in its own place.
    Next(Request),
}

/// Speak what the queue asks for, until something says otherwise.
///
/// A `Request::Loop` is the only thing asked for more than once: when an attempt reaches its end and
/// nothing newer is queued, the same line is asked for again from the live settings, so the
/// temperature on screen is the temperature heard in the next sentence. Between utterances the loop
/// blocks on the queue and only the drain looks at it, so an idle extension costs nothing.
///
/// `speak` is the binary's turn: it owns the model, the player and the reports, and the receiver its
/// own cooperative control point reads is the one this loop drains. A request that control point took
/// out of the queue mid-utterance arrives back as `Outcome::Next`, which is how a superseding request
/// or an `Exit` reaches this loop.
pub fn speak_queue(
    rx: &Receiver<Request>,
    live: &Arc<Mutex<Settings>>,
    speak: &mut dyn FnMut(&Utterance) -> Outcome,
) {
    // What to speak next: the repetition a finished line earned, or a request the pipeline stopped
    // for. Everything else comes off the queue.
    let mut pending: Option<Request> = None;
    loop {
        let request = match pending.take() {
            Some(request) => request,
            None => match rx.recv() {
                Ok(request) => request,
                Err(_) => return, // every sender is gone: nobody left to ask
            },
        };
        let request = match coalesce(rx, request) {
            Request::Stop => continue, // the queue had the last word: go quiet
            Request::Exit => return,
            request => request,
        };
        // The emptiness decision is taken here, before the binary is asked for an utterance: a line
        // with nothing a voice can read costs no noise seed and no warning about a missing voice.
        let Some(utterance) = utterance_for(&request, live) else {
            continue;
        };
        let repeating = matches!(&utterance, Utterance::Loop { .. });
        match speak(&utterance) {
            // A repetition is the only thing asked for twice, `Finished` the only state it continues
            // from, and the breath comes first so the next line lands as a sentence and not a stutter.
            Outcome::Finished if repeating => {
                thread::sleep(LOOP_GAP);
                pending = Some(request); // the request as the queue got it: its raw text and its voice
            }
            // Cut short by a newer request, which decides what follows, repeating included.
            Outcome::Next(next) => pending = Some(next),
            Outcome::Finished | Outcome::Quiet => {}
        }
    }
}

/// A request superseded mid-stream is usually a held arrow key in the picker: one preview per step,
/// all of them now pointless. Drain the queue and keep only the newest request, so the voice under
/// the cursor is the one you hear instead of a backlog of voices you walked past. A drain that found
/// nothing hands back the request it was given, which is what makes a repeat keep repeating.
fn coalesce(rx: &Receiver<Request>, request: Request) -> Request {
    let mut latest = request;
    loop {
        match rx.try_recv() {
            Ok(next) => latest = next,
            Err(TryRecvError::Empty) => return latest,
            Err(TryRecvError::Disconnected) => return Request::Exit,
        }
    }
}

/// What one request amounts to as an utterance: the text with everything a voice cannot read taken
/// out, and the settings it is spoken at. `None` when nothing is left to say. A repetition is spoken
/// at the live settings and keeps the voice its explanation started in; a one-shot keeps the snapshot
/// its sender took.
fn utterance_for(request: &Request, live: &Arc<Mutex<Settings>>) -> Option<Utterance> {
    let (text, settings, repeat) = match request {
        Request::Play { text, settings } => (text, settings.clone(), false),
        Request::Loop { text, voice } => (text, live_settings(live, voice), true),
        Request::Stop | Request::Exit => return None,
    };
    let spoken = tts::spoken_text(text);
    if spoken.is_empty() {
        // Nothing in it a voice could say: an answer that is only a code block or only a table goes
        // quiet rather than feeding the model an empty request.
        return None;
    }
    Some(if repeat {
        Utterance::Loop { spoken, settings }
    } else {
        Utterance::Play { spoken, settings }
    })
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

#[cfg(test)]
mod tests {
    use super::{
        coalesce, live_settings, speak_queue, utterance_for, Outcome, Request, Settings, Utterance,
    };
    use std::sync::mpsc::{self, Sender};
    use std::sync::{Arc, Mutex};

    /// A repeat repeats because a drain that found nothing hands back the request it was given: the
    /// queue is the only thing that ends it. So the queue's ways of ending one have to win in the
    /// right order — a Stop, a newer request, an Exit.
    #[test]
    fn coalescing_repeats_until_the_queue_says_otherwise() {
        let repeat = || Request::Loop {
            text: "one line".to_string(),
            voice: "Miles".to_string(),
        };
        let (tx, rx) = mpsc::channel();
        assert!(
            matches!(coalesce(&rx, repeat()), Request::Loop { .. }),
            "an empty drain must hand the repeat back, not drop it"
        );
        tx.send(Request::Stop).unwrap();
        assert!(
            matches!(coalesce(&rx, repeat()), Request::Stop),
            "a Stop ends a repeat"
        );
        tx.send(Request::Loop {
            text: "another line".to_string(),
            voice: "Zoey".to_string(),
        })
        .unwrap();
        match coalesce(&rx, repeat()) {
            Request::Loop { text, voice } => {
                assert_eq!((text.as_str(), voice.as_str()), ("another line", "Zoey"))
            }
            _ => panic!("the newest request should win over the one it supersedes"),
        }
        tx.send(Request::Exit).unwrap();
        assert!(
            matches!(coalesce(&rx, repeat()), Request::Exit),
            "exit outranks a repeat"
        );
        // The drain itself is what the loop blocks on between utterances, so a sender that is
        // still alive must never look like an ended conversation.
        let held: Sender<Request> = tx.clone();
        assert!(matches!(coalesce(&rx, repeat()), Request::Loop { .. }));
        drop(held);
        drop(tx);
        assert!(
            matches!(coalesce(&rx, repeat()), Request::Exit),
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

    /// The emptiness decision belongs to the queue and not to the pipeline: what a voice cannot read
    /// becomes no utterance at all, so it never reaches the code that draws a noise seed or warns
    /// about a voice that is not registered.
    #[test]
    fn a_line_a_voice_cannot_read_becomes_no_utterance() {
        let live = Arc::new(Mutex::new(Settings::default()));
        let request = |text: &str| Request::Play {
            text: text.to_string(),
            settings: Settings::default(),
        };
        for text in [
            "```rust\nlet x = foo(bar);\n```",
            "| name | value |\n|---|---|\n| speed | 1.0x |",
            "`a line that is only a span`",
            "   \n  ",
        ] {
            assert!(
                utterance_for(&request(text), &live).is_none(),
                "{text:?} holds nothing a voice can read"
            );
        }
        let Some(Utterance::Play { spoken, settings }) =
            utterance_for(&request("## The gates\n\nRead this aloud."), &live)
        else {
            panic!("a line a voice can read has to become an utterance");
        };
        assert_eq!(spoken, "The gates. Read this aloud.");
        assert_eq!(
            settings,
            Settings::default(),
            "a one-shot keeps its snapshot"
        );
    }

    /// The loop with a stand-in for the pipeline, no model and no player: a finished line that was
    /// asked for on repeat is asked for again at the settings on screen, in the voice the explanation
    /// started in, and only the queue ends it.
    #[test]
    fn a_finished_line_repeats_until_the_queue_ends_it() {
        let (tx, rx) = mpsc::channel();
        let live = Arc::new(Mutex::new(Settings {
            temperature: 0.20,
            ..Settings::default()
        }));
        tx.send(Request::Loop {
            text: "one line".to_string(),
            voice: "Miles".to_string(),
        })
        .unwrap();
        // The test's way of ending the loop from inside the stand-in.
        let ending = tx.clone();
        let heard = Arc::new(Mutex::new(Vec::new()));
        let recording = heard.clone();
        speak_queue(&rx, &live, &mut |utterance| {
            let Utterance::Loop { spoken, settings } = utterance else {
                panic!("a repeat request is spoken as a repeat utterance");
            };
            let attempt = {
                let mut heard = recording.lock().unwrap();
                heard.push((spoken.clone(), settings.voice.clone(), settings.temperature));
                heard.len()
            };
            if attempt == 1 {
                // The user steps the Temperature row while the first sentence is being said.
                live.lock().unwrap().temperature = 0.80;
            }
            if attempt == 3 {
                let _ = ending.send(Request::Exit); // the panel moved off the row
            }
            Outcome::Finished
        });

        let heard = heard.lock().unwrap();
        assert_eq!(
            heard.len(),
            3,
            "a finished repeat goes on until the queue ends it"
        );
        assert!(
            heard
                .iter()
                .all(|(spoken, voice, _)| spoken == "one line" && voice == "Miles"),
            "every repetition says the same line in the voice it started in: {heard:?}"
        );
        assert!(
            (heard[0].2 - 0.20).abs() < 1e-6 && (heard[1].2 - 0.80).abs() < 1e-6,
            "the second repetition hears the value the row moved to: {heard:?}"
        );
    }

    /// A one-shot is one utterance: finishing it earns no repetition, whatever the queue looks like
    /// afterwards.
    #[test]
    fn a_finished_one_shot_is_not_spoken_again() {
        let (tx, rx) = mpsc::channel();
        let live = Arc::new(Mutex::new(Settings::default()));
        tx.send(Request::Play {
            text: "one line".to_string(),
            settings: Settings::default(),
        })
        .unwrap();
        let ending = tx.clone();
        let attempts = Arc::new(Mutex::new(0));
        let counting = attempts.clone();
        speak_queue(&rx, &live, &mut |utterance| {
            *counting.lock().unwrap() += 1;
            assert!(matches!(utterance, Utterance::Play { .. }));
            let _ = ending.send(Request::Exit);
            Outcome::Finished
        });
        assert_eq!(*attempts.lock().unwrap(), 1, "a one-shot is spoken once");
    }

    /// What the pipeline took out of the queue on its way out is the queue's next word, and the line it
    /// cut short earns no repetition. A request already drained cannot be put back where it came from,
    /// so this is the shape that carries it, and it is what the panel's cursor moves depend on: the
    /// voice you stopped on is spoken even though nothing is queued behind it.
    #[test]
    fn a_request_taken_mid_utterance_is_the_next_one_spoken() {
        let (tx, rx) = mpsc::channel();
        let live = Arc::new(Mutex::new(Settings::default()));
        tx.send(Request::Loop {
            text: "first line".to_string(),
            voice: "Miles".to_string(),
        })
        .unwrap();
        let heard = Arc::new(Mutex::new(Vec::new()));
        let recording = heard.clone();
        let ending = tx.clone();
        speak_queue(&rx, &live, &mut |utterance| {
            let (kind, spoken) = match utterance {
                Utterance::Loop { spoken, .. } => ("loop", spoken.clone()),
                Utterance::Play { spoken, .. } => ("play", spoken.clone()),
            };
            let attempt = {
                let mut heard = recording.lock().unwrap();
                heard.push(format!("{kind}: {spoken}"));
                heard.len()
            };
            if attempt == 1 {
                // A cursor move while this line is being said: the pipeline drained that request off
                // the queue, so it reports it instead of speaking it, and nothing is left behind it.
                return Outcome::Next(Request::Play {
                    text: "second line".to_string(),
                    settings: Settings::default(),
                });
            }
            let _ = ending.send(Request::Exit);
            Outcome::Finished
        });

        let heard = heard.lock().unwrap();
        assert_eq!(
            heard.as_slice(),
            ["loop: first line", "play: second line"],
            "the line cut short is not repeated, and the request taken for is spoken next"
        );
    }

    /// An `Exit` the pipeline took is a report the loop acts on, not a state it goes back to waiting
    /// in: the request is already off the queue, and waiting for another one would outlive the process
    /// that sent it.
    #[test]
    fn an_exit_taken_mid_utterance_ends_the_loop() {
        let (tx, rx) = mpsc::channel();
        let live = Arc::new(Mutex::new(Settings::default()));
        tx.send(Request::Play {
            text: "one line".to_string(),
            settings: Settings::default(),
        })
        .unwrap();
        let attempts = Arc::new(Mutex::new(0));
        let counting = attempts.clone();
        speak_queue(&rx, &live, &mut |utterance| {
            assert!(matches!(utterance, Utterance::Play { .. }));
            *counting.lock().unwrap() += 1;
            Outcome::Next(Request::Exit)
        });
        assert_eq!(
            *attempts.lock().unwrap(),
            1,
            "the loop stops on an Exit it was told about"
        );
    }
}
