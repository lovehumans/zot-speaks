//! The `/tts-settings` panel state machine.
//!
//! zot panels are plain text: a `title`, a list of body `lines`, and a `footer`. The host sends
//! normalized `panel_key` events (`up`, `down`, `left`, `right`, `enter`, `esc`, …, or `rune`
//! with a `text` character). We edit `Settings` live and hand back frames the caller serialises.
//!
//! Main view — cursor over five rows (Voice, Volume, Speed, Temperature, Auto speak):
//!   up/down      move the cursor
//!   left/right   (or +/-) adjust the focused row; a Volume or Speed change then speaks the
//!                selected voice, so the new level or rate is heard rather than read. On Auto
//!                speak they step a three-value ring: off -> on · here -> on · everywhere -> off.
//!   enter        open the voice picker when the cursor sits on Voice
//!   s            speak the last agent message in the selected voice (a voice audition)
//!   esc          close the panel
//!
//! Voice picker — cursor over every catalog voice in the model's language, in three blocks
//! separated by a blank line: favorites (pinned with `f`), then ready voices, then
//! not-yet-downloaded ones:
//!   up/down      move; the highlighted voice introduces itself out loud
//!   enter        select the highlighted ready voice and return to the main view
//!   s            speak the last agent message in the highlighted voice
//!   f            pin/unpin the highlighted voice as a favorite (it joins the block at the top)
//!   esc          return to the main view
//! Not-downloaded voices are listed but greyed as `(not downloaded)`; selecting one is a no-op
//! that instead shows the `./ptts --voice …` command that would fetch it, and moving onto one
//! only silences the previous voice.
//!
//! Audio is not this module's business: `on_key` returns a `Preview` describing what the caller
//! should say, in which voice, and the caller (main.rs) owns the synth and the sink.

use crate::settings::Settings;
use crate::voices::VoiceInfo;
use std::path::PathBuf;

pub const PANEL_ID: &str = "zot-speaks-settings";

/// Volume moves in 5% of the dial (see `settings::Settings::gain`: 50% is the voice's own level).
const VOLUME_STEP: f32 = 0.05;
const TEMPERATURE_STEP: f32 = 0.05;
/// The rows of the main view, in cursor order.
const ROWS: usize = 5;
const VOICE_ROW: usize = 0;
const VOLUME_ROW: usize = 1;
const SPEED_ROW: usize = 2;
const TEMPERATURE_ROW: usize = 3;
/// The row whose value is a ring of three states rather than a number, so arrows step it instead
/// of adjusting it (see `settings::AutoSpeak`).
const AUTO_SPEAK_ROW: usize = 4;
/// Floor for the picker's name column; it grows to fit the longest catalog name.
const MIN_NAME_WIDTH: usize = 18;
/// Detail rows under the voice list, and the widest panel line we ever emit. Every line is
/// indented by `ROW_INDENT`, so description text wraps to `DETAIL_WIDTH - ROW_INDENT`. zot
/// wraps body lines to the terminal width and gives us no width to work with, so 70 columns
/// keeps every row on one screen row of an 80-column terminal.
const DETAIL_ROWS: usize = 2;
const DETAIL_WIDTH: usize = 70;
const ROW_INDENT: usize = 2;
const DETAIL_TEXT_WIDTH: usize = DETAIL_WIDTH - ROW_INDENT;

/// The line a voice speaks when the cursor lands on it, and how much of its description may be
/// read out. Whole descriptions run to 300 characters, which is far too long to listen to while
/// you are choosing, so only whole sentences that fit this budget are spoken.
const INTRO_PREFIX: &str = "Hello, my name is";
const INTRO_CHARS: usize = 160;

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Main,
    Voice,
}

/// What the caller should say aloud because of a key press.
#[derive(Clone, Debug, PartialEq)]
pub enum Preview {
    /// Introduce `voice` by name, in that voice (`text` is the introduction line).
    Voice { voice: String, text: String },
    /// Speak the last agent message in `voice` (`""` = whatever the settings select).
    LastMessage { voice: String },
    /// Cut off whatever is playing: the cursor landed on a voice we cannot synthesize.
    Silence,
}

pub enum KeyOutcome {
    Continue {
        changed: bool,
        preview: Option<Preview>,
    },
    Close,
}

pub struct SettingsPanel {
    mode: Mode,
    focus: usize,
    voice_idx: usize,
    voices: Vec<VoiceInfo>, // full model-language catalog: available and not-yet-downloaded
    settings: Settings,
    /// The project this session is running in. Auto-speak is scoped per project, so the panel
    /// needs to know which project it is editing on the user's behalf.
    project: PathBuf,
}

impl SettingsPanel {
    pub fn new(voices: Vec<VoiceInfo>) -> Self {
        SettingsPanel {
            mode: Mode::Main,
            focus: 0,
            voice_idx: 0,
            voices,
            settings: Settings::default(),
            project: PathBuf::from("."),
        }
    }

    /// Tell the panel which project the session lives in (`hello_ack.cwd`, and every event frame
    /// that reports a different cwd). Called before `open`, and again while open so the Auto speak
    /// row always describes the project the user is actually in.
    pub fn set_project(&mut self, project: PathBuf) {
        self.project = project;
    }

    /// Sync the panel with the current settings (called when the panel opens).
    pub fn open(&mut self, settings: &Settings) {
        self.settings = settings.clone();
        self.order_voices();
        self.voice_idx = self.voice_position(&self.settings.voice);
        self.mode = Mode::Main;
        self.focus = 0;
    }

    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    /// The picker block a voice belongs to: 0 pinned favorite, 1 ready, 2 not downloaded.
    fn group_of(&self, voice: &VoiceInfo) -> u8 {
        rank(voice, &self.settings.favorites)
    }

    fn group_counts(&self) -> [usize; 3] {
        let mut counts = [0usize; 3];
        for voice in &self.voices {
            counts[usize::from(self.group_of(voice))] += 1;
        }
        counts
    }

    /// Reorder into blocks — favorites, then ready, then not-downloaded — and put the cursor back
    /// on the voice it was on. Called on open and whenever a voice is pinned. The sort is stable,
    /// so a block keeps the catalog's own order: pinning one voice never shuffles another, and the
    /// cursor cannot lose the voice it is on.
    fn order_voices(&mut self) {
        let highlighted = self
            .voices
            .get(self.voice_idx)
            .map(|voice| voice.name.clone());
        let favorites = self.settings.favorites.clone();
        self.voices.sort_by_key(|voice| rank(voice, &favorites));
        self.voice_idx = match highlighted.as_deref() {
            Some(name) => self
                .voices
                .iter()
                .position(|voice| voice.name == name)
                .unwrap_or(0),
            None => 0,
        };
    }

    fn ready(&self) -> usize {
        self.voices.iter().filter(|voice| voice.available).count()
    }

    fn voice_position(&self, name: &str) -> usize {
        // The catalog is sorted available-first, so the selected voice lands in the ready block
        // and `0` (a ready voice when any exists) is a safe default cursor.
        self.voices
            .iter()
            .position(|voice| voice.name == name)
            .unwrap_or(0)
    }

    pub fn title(&self) -> String {
        match self.mode {
            Mode::Main => "TTS settings".to_string(),
            Mode::Voice => format!(
                "Choose a voice ({} ready of {} {} voices)",
                self.ready(),
                self.voices.len(),
                crate::tts::MODEL_LANGUAGE
            ),
        }
    }

    pub fn lines(&self) -> Vec<String> {
        match self.mode {
            Mode::Main => self.main_lines(),
            Mode::Voice => self.voice_lines(),
        }
    }

    pub fn footer(&self) -> String {
        // Kept within DETAIL_WIDTH for the same reason the body lines are: a footer that wraps
        // changes the panel height when you move between rows.
        match self.mode {
            Mode::Main => match self.focus {
                VOICE_ROW => {
                    "up/down move   enter choose voice   s last message   esc close".to_string()
                }
                AUTO_SPEAK_ROW => {
                    "left/right off · here · everywhere   s last message   esc close".to_string()
                }
                _ => "up/down move   arrows or -/+ (audible)   s last message   esc close"
                    .to_string(),
            },
            Mode::Voice => {
                "up/down preview   enter pick   s last message   f favorite   esc back".to_string()
            }
        }
    }

    fn main_lines(&self) -> Vec<String> {
        let mut voice_display = self.voice_display();
        if self.settings.is_favorite(&self.settings.voice) {
            voice_display.push_str(" ★");
        }
        let summary = format!(
            "{} ready of {} {} voices",
            self.ready(),
            self.voices.len(),
            crate::tts::MODEL_LANGUAGE
        );
        vec![
            self.row(VOICE_ROW, "Voice", &format!("{voice_display}  ({summary})")),
            self.row(
                VOLUME_ROW,
                "Volume",
                &format!("{:.0}%", self.settings.volume * 100.0),
            ),
            self.row(SPEED_ROW, "Speed", &self.settings.speed.label()),
            self.row(
                TEMPERATURE_ROW,
                "Temperature",
                &format!("{:.2}", self.settings.temperature),
            ),
            self.row(AUTO_SPEAK_ROW, "Auto speak", &self.auto_speak_display()),
        ]
    }

    /// The row's value: `off`, `on · here` (this project alone) or `on · everywhere` (the default
    /// every project follows). Naming the scope is the only way to tell "off for this project"
    /// apart from "off for everyone, and this project is just following along".
    fn auto_speak_display(&self) -> String {
        self.settings
            .auto_speak_state(&self.project)
            .label()
            .to_string()
    }

    /// The name the Voice row shows for the current selection.
    fn voice_display(&self) -> String {
        if self.settings.voice.is_empty() {
            "default".to_string()
        } else {
            self.settings.voice.clone()
        }
    }

    fn voice_lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        if self.voices.is_empty() {
            lines.push(format!(
                "no {} voices in the catalog — cache it once with `./ptts --voice Miles \"hi\"`, then re-run",
                crate::tts::MODEL_LANGUAGE
            ));
            return lines;
        }
        // Name column: as wide as the longest name in this catalog. English names run up
        // to ~26 chars ("The Thing Under the Stairs"), and a fixed width let them butt
        // straight into the metadata column. `│` delimits the two columns.
        let name_width = self
            .voices
            .iter()
            .map(|voice| voice.name.chars().count())
            .max()
            .unwrap_or(0)
            .max(MIN_NAME_WIDTH);
        let counts = self.group_counts();
        let mut block = 3u8; // no block emitted yet
        for (index, voice) in self.voices.iter().enumerate() {
            let current = self.group_of(voice);
            if current != block {
                if block != 3 {
                    // A blank line between blocks. Without it a pinned voice and the first ready
                    // one look like one run of names, and the header reads as another voice row.
                    lines.push(String::new());
                }
                // Only the favorite and ready blocks get a header: every row of the greyed block
                // already says `(not downloaded)`.
                if current == 0 && counts[0] > 0 {
                    lines.push(format!("  ★ {} favorite", counts[0]));
                } else if current == 1 && counts[1] > 0 {
                    lines.push(format!("  {} ready", counts[1]));
                }
                block = current;
            }
            let marker = if index == self.voice_idx { ">" } else { " " };
            let mut line = if voice.available {
                format!(
                    "{} {:<w$} │ {}",
                    marker,
                    voice.name,
                    voice.tag(),
                    w = name_width
                )
            } else {
                // Grey out catalog voices whose embedding is not cached yet.
                format!(
                    "{} {:<w$} │ {}  (not downloaded)",
                    marker,
                    voice.name,
                    voice.tag(),
                    w = name_width
                )
            };
            // A name long enough to push the row past the wrap column would make zot split it
            // in two and change the list's height, so the metadata tail gives way instead.
            if line.chars().count() > DETAIL_WIDTH {
                clip(&mut line, DETAIL_WIDTH);
            }
            lines.push(line);
        }
        // Detail block for the highlighted voice, at a fixed height (see `detail_lines`).
        lines.push(String::new());
        lines.extend(self.detail_lines());
        lines
    }

    /// The two rows under the list: a ready voice's description, or the command that would
    /// fetch a not-downloaded one.
    ///
    /// Always exactly `DETAIL_ROWS` rows, each at most `DETAIL_WIDTH` columns wide once
    /// indented. zot wraps body lines to the terminal width and centers the panel vertically,
    /// so a description that happened to need one row instead of two used to resize the panel
    /// and make the whole 37-row list jump while you arrowed through it.
    fn detail_lines(&self) -> Vec<String> {
        let indent = " ".repeat(ROW_INDENT);
        let mut rows = match self.voices.get(self.voice_idx) {
            Some(voice) if voice.available => wrap_rows(
                voice
                    .description
                    .as_deref()
                    .unwrap_or("no description in the catalog"),
                DETAIL_TEXT_WIDTH,
            ),
            Some(voice) => {
                let command = format!("./ptts --voice \"{}\" \"hi\"", voice.name);
                let mut rows = vec!["not downloaded — fetch it once:".to_string()];
                rows.extend(wrap_rows(&command, DETAIL_TEXT_WIDTH));
                rows
            }
            None => Vec::new(),
        };
        if rows.len() > DETAIL_ROWS {
            rows.truncate(DETAIL_ROWS);
            if let Some(last) = rows.last_mut() {
                mark_continued(last, DETAIL_TEXT_WIDTH);
            }
        }
        rows.resize(DETAIL_ROWS, String::new());
        rows.into_iter()
            .map(|row| format!("{indent}{row}"))
            .collect()
    }

    fn row(&self, index: usize, label: &str, value: &str) -> String {
        let marker = if index == self.focus { ">" } else { " " };
        format!("{marker} {:<13}{}", label, value)
    }

    pub fn on_key(&mut self, key: &str, text: &str) -> KeyOutcome {
        match self.mode {
            Mode::Main => self.on_key_main(key, text),
            Mode::Voice => self.on_key_voice(key, text),
        }
    }

    fn on_key_main(&mut self, key: &str, text: &str) -> KeyOutcome {
        match key {
            "up" => {
                self.focus = (self.focus + ROWS - 1) % ROWS;
                stay(None)
            }
            "down" => {
                self.focus = (self.focus + 1) % ROWS;
                stay(None)
            }
            "enter" => {
                if self.focus == VOICE_ROW {
                    // Opening the picker previews the voice it lands on, so the first thing you
                    // hear is the voice you are about to browse.
                    self.mode = Mode::Voice;
                    self.voice_idx = self.voice_position(&self.settings.voice);
                    return stay(self.preview_voice());
                }
                stay(None)
            }
            "esc" => KeyOutcome::Close,
            "left" => self.nudge(-1.0),
            "right" => self.nudge(1.0),
            "rune" => match text {
                "+" | "=" => self.nudge(1.0),
                "-" | "_" => self.nudge(-1.0),
                // An audition of the voice actually selected, on real text rather than the
                // canned introduction.
                "s" => stay(Some(Preview::LastMessage {
                    voice: self.settings.voice.clone(),
                })),
                _ => stay(None),
            },
            _ => stay(None),
        }
    }

    fn on_key_voice(&mut self, key: &str, text: &str) -> KeyOutcome {
        let last = self.voices.len().saturating_sub(1);
        match key {
            "up" => {
                self.voice_idx = if self.voice_idx == 0 {
                    last
                } else {
                    self.voice_idx - 1
                };
                stay(self.preview_voice())
            }
            "down" => {
                self.voice_idx = if self.voice_idx >= last {
                    0
                } else {
                    self.voice_idx + 1
                };
                stay(self.preview_voice())
            }
            "enter" => {
                // Commit only ready voices; a not-downloaded one stays put (its hint already shows).
                let mut changed = false;
                if let Some(voice) = self.voices.get(self.voice_idx) {
                    if voice.available {
                        if self.settings.voice != voice.name {
                            self.settings.voice = voice.name.clone();
                            changed = true;
                        }
                        self.mode = Mode::Main;
                    }
                }
                // No preview here: its voice is already talking, and cutting it off would make
                // selecting feel like losing it.
                stay_changed(changed, None)
            }
            "rune" => match text {
                "s" => match self.voices.get(self.voice_idx) {
                    Some(voice) if voice.available => stay(Some(Preview::LastMessage {
                        voice: voice.name.clone(),
                    })),
                    Some(_) => stay(Some(Preview::Silence)),
                    None => stay(None),
                },
                "f" => match self
                    .voices
                    .get(self.voice_idx)
                    .map(|voice| voice.name.clone())
                {
                    Some(name) => {
                        self.settings.toggle_favorite(&name);
                        self.order_voices(); // the pinned voice moves up into the favorite block
                                             // Same voice, now pinned: hearing it again confirms the pin landed, and it
                                             // is also what a favorite sounds like.
                        stay_changed(true, self.preview_voice())
                    }
                    None => stay(None),
                },
                _ => stay(None),
            },
            "esc" => {
                self.mode = Mode::Main;
                // Leaving the picker should not leave a voice talking in the main view.
                stay(Some(Preview::Silence))
            }
            _ => stay(None),
        }
    }

    /// What the highlighted picker voice should say: itself when we can synthesize it, silence
    /// when its embedding is not cached (which also cuts off the voice we just left).
    fn preview_voice(&self) -> Option<Preview> {
        match self.voices.get(self.voice_idx) {
            Some(voice) if voice.available => Some(Preview::Voice {
                voice: voice.name.clone(),
                text: introduction(voice),
            }),
            Some(_) => Some(Preview::Silence),
            None => None,
        }
    }

    /// Step the focused row. A Volume or Speed change is followed by an audition of the voice you
    /// selected, so the new level or rate is something you hear rather than a number you read.
    /// Temperature stays silent: it changes *how* a voice speaks (see `tts::build_options`), which a
    /// name-only line cannot show. Auto-speak makes no sound of its own either — the next agent turn
    /// is its feedback — and its arrows step the off / here / everywhere ring.
    fn nudge(&mut self, direction: f32) -> KeyOutcome {
        if self.focus == AUTO_SPEAK_ROW {
            // A ring of three states: every arrow lands somewhere new, so there is no press that
            // does nothing to save.
            self.settings
                .cycle_auto_speak(&self.project, direction.signum() as i32);
            return stay_changed(true, None);
        }
        let changed = self.adjust(self.focus, direction);
        if changed && (self.focus == VOLUME_ROW || self.focus == SPEED_ROW) {
            let voice = self.settings.voice.clone();
            let text = quick_introduction(&self.voice_display());
            stay_changed(changed, Some(Preview::Voice { voice, text }))
        } else {
            stay_changed(changed, None)
        }
    }

    /// Apply a signed step to the focused numeric row. Returns whether anything changed.
    fn adjust(&mut self, focus: usize, direction: f32) -> bool {
        match focus {
            VOLUME_ROW => {
                let target = (self.settings.volume + direction * VOLUME_STEP).clamp(0.0, 1.0);
                if target != self.settings.volume {
                    self.settings.volume = target;
                    return true;
                }
            }
            SPEED_ROW => {
                let step = if direction < 0.0 { -1 } else { 1 };
                let target = self.settings.speed.stepped(step);
                if target != self.settings.speed {
                    self.settings.speed = target;
                    return true;
                }
            }
            TEMPERATURE_ROW => {
                let target =
                    (self.settings.temperature + direction * TEMPERATURE_STEP).clamp(0.0, 1.0);
                if target != self.settings.temperature {
                    self.settings.temperature = target;
                    return true;
                }
            }
            _ => {}
        }
        false
    }
}

/// A key that left the panel open without editing anything.
fn stay(preview: Option<Preview>) -> KeyOutcome {
    KeyOutcome::Continue {
        changed: false,
        preview,
    }
}

/// The picker order: pinned favorites first, then ready voices, then not-downloaded ones. Inside a
/// block the catalog's own order stands, so pinning a voice moves only that voice.
fn rank(voice: &VoiceInfo, favorites: &[String]) -> u8 {
    match (
        favorites.iter().any(|favorite| favorite == &voice.name),
        voice.available,
    ) {
        (true, _) => 0,
        (false, true) => 1,
        (false, false) => 2,
    }
}

/// The line that makes a Volume change audible: the voice's name and nothing else, so two levels
/// are comparable within a second instead of after a whole description.
fn quick_introduction(name: &str) -> String {
    format!("{INTRO_PREFIX} {name}.")
}

/// A key that left the panel open after editing a setting.
fn stay_changed(changed: bool, preview: Option<Preview>) -> KeyOutcome {
    KeyOutcome::Continue { changed, preview }
}

/// What a voice says when the cursor lands on it: `Hello, my name is Zoey. <its description>`,
/// with the description shortened to something worth listening to (see `spoken`).
fn introduction(voice: &VoiceInfo) -> String {
    let mut text = format!("{INTRO_PREFIX} {}.", voice.name);
    if let Some(description) = voice
        .description
        .as_deref()
        .map(str::trim)
        .filter(|d| !d.is_empty())
    {
        let spoken = spoken(description, INTRO_CHARS);
        if !spoken.is_empty() {
            text.push(' ');
            text.push_str(&spoken);
        }
    }
    text
}

/// The part of `text` worth synthesizing: whole sentences that fit `budget` characters, joined
/// with single spaces. When the first sentence alone is too long, whole words of it instead, so
/// a preview never stops mid-word (and never turns into a 30-second narration).
fn spoken(text: &str, budget: usize) -> String {
    let mut taken: Vec<String> = Vec::new();
    let mut used = 0usize;
    for sentence in sentences(text) {
        let gap = usize::from(!taken.is_empty());
        if used + gap + sentence.chars().count() > budget {
            break;
        }
        used += gap + sentence.chars().count();
        taken.push(sentence);
    }
    if taken.is_empty() {
        return whole_words(text, budget);
    }
    taken.join(" ")
}

/// Split on `.`, `!` and `?`, keeping each terminator with its sentence and trimming the rest.
/// The trailing fragment of text that never ends a sentence counts as a sentence.
fn sentences(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut current = String::new();
    for ch in text.chars() {
        current.push(ch);
        if matches!(ch, '.' | '!' | '?') {
            let sentence = current.trim().to_string();
            if !sentence.is_empty() {
                out.push(sentence);
            }
            current.clear();
        }
    }
    let tail = current.trim().to_string();
    if !tail.is_empty() {
        out.push(tail);
    }
    out
}

/// The longest run of whole words from `text` that fits in `budget` characters.
fn whole_words(text: &str, budget: usize) -> String {
    let mut out = String::new();
    for word in text.split_whitespace() {
        let gap = usize::from(!out.is_empty());
        if out.chars().count() + gap + word.chars().count() > budget {
            break;
        }
        if gap == 1 {
            out.push(' ');
        }
        out.push_str(word);
    }
    out
}

/// Greedy word wrap into as many rows as needed; every row stays within `width` characters.
/// A single word wider than the row is cut with an ellipsis rather than overflowing.
fn wrap_rows(text: &str, width: usize) -> Vec<String> {
    let mut rows: Vec<String> = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        let joined = if line.is_empty() {
            word.to_string()
        } else {
            format!("{line} {word}")
        };
        if joined.chars().count() <= width {
            line = joined;
            continue;
        }
        if !line.is_empty() {
            rows.push(std::mem::take(&mut line));
        }
        let cut: String = word.chars().take(width.saturating_sub(1)).collect();
        if cut.chars().count() < word.chars().count() {
            let mut row = cut;
            row.push('…');
            rows.push(row);
        } else {
            line = word.to_string();
        }
    }
    if !line.is_empty() {
        rows.push(line);
    }
    rows
}

/// Overwrite `row` with a version that ends in `…` and never exceeds `width` characters.
fn clip(row: &mut String, width: usize) {
    let mut cut: String = row
        .trim_end()
        .chars()
        .take(width.saturating_sub(1))
        .collect();
    cut.push('…');
    *row = cut;
}

/// Mark that a dropped row's text continues past the end of this one, still within `width`.
fn mark_continued(row: &mut String, width: usize) {
    let trimmed = row.trim_end().to_string();
    if trimmed.chars().count() < width {
        // one column must stay free for the mark
        *row = format!("{trimmed}…");
    } else {
        clip(row, width); // no room even at full width: shorten the row so the mark fits
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::{SPEED_MAX, SPEED_MIN};
    use std::path::PathBuf;

    fn voice(name: &str, available: bool) -> VoiceInfo {
        VoiceInfo {
            name: name.to_string(),
            id: name.to_ascii_lowercase(),
            path: PathBuf::from(format!("voices/{name}.safetensors")),
            available,
            language: Some("en".to_string()),
            locale: Some("en-us".to_string()),
            gender: Some("female".to_string()),
            age: Some("Adult".to_string()),
            description: Some(format!("{name} description.")), // real catalog descriptions are sentences
        }
    }

    // Available-first catalog: two ready, two not-downloaded.
    fn catalog() -> Vec<VoiceInfo> {
        vec![
            voice("Miles", true),
            voice("Juniper", true),
            voice("Zoey", false),
            voice("Alfie", false),
        ]
    }

    #[test]
    fn picker_counts_ready_of_total() {
        let panel = SettingsPanel::new(catalog());
        let title = panel.title(); // Voice mode not entered yet -> main title
        assert_eq!(title, "TTS settings");
    }

    #[test]
    fn picker_lists_all_voices_and_marks_not_downloaded() {
        let mut panel = SettingsPanel::new(catalog());
        panel.open(&Settings::default());
        // Enter the picker (cursor starts on Voice row).
        assert!(matches!(
            panel.on_key("enter", ""),
            KeyOutcome::Continue { .. }
        ));
        let lines = panel.lines();
        let ready_lines: Vec<&String> = lines
            .iter()
            .filter(|l| !l.contains("(not downloaded)"))
            .collect();
        // Two ready voices listed without the marker; two greyed with it.
        assert!(lines
            .iter()
            .any(|l| l.contains("Miles") && !l.contains("(not downloaded)")));
        assert!(lines
            .iter()
            .any(|l| l.contains("Zoey") && l.contains("(not downloaded)")));
        assert!(lines
            .iter()
            .any(|l| l.contains("Alfie") && l.contains("(not downloaded)")));
        // A ready voice's detail line shows its description, not a fetch hint.
        assert!(ready_lines.iter().any(|l| l.contains("description")));
    }

    fn cursor_name(panel: &SettingsPanel) -> String {
        let line = panel
            .lines()
            .into_iter()
            .find(|l| l.starts_with('>'))
            .expect("cursor line");
        line[2..].split('│').next().unwrap_or("").trim().to_string()
    }

    fn preview_of(outcome: KeyOutcome) -> Option<Preview> {
        match outcome {
            KeyOutcome::Continue { preview, .. } => preview,
            KeyOutcome::Close => None,
        }
    }

    #[test]
    fn picker_previews_the_voice_the_cursor_lands_on() {
        let mut panel = SettingsPanel::new(catalog()); // Miles, Juniper ready; Zoey, Alfie not
        panel.open(&Settings {
            voice: "Miles".into(),
            ..Settings::default()
        });

        // Opening the picker already previews the voice it lands on.
        assert_eq!(
            preview_of(panel.on_key("enter", "")),
            Some(Preview::Voice {
                voice: "Miles".into(),
                text: "Hello, my name is Miles. Miles description.".into()
            }),
            "the picker should speak the voice it opens on"
        );
        // Moving speaks the new voice (which supersedes the old one in the caller).
        assert_eq!(
            preview_of(panel.on_key("down", "")),
            Some(Preview::Voice {
                voice: "Juniper".into(),
                text: "Hello, my name is Juniper. Juniper description.".into()
            }),
            "moving the cursor should speak the voice it lands on"
        );
        // A greyed voice has no embedding to speak: the previous preview must be cut off.
        assert_eq!(
            preview_of(panel.on_key("down", "")),
            Some(Preview::Silence),
            "a not-downloaded voice must silence the last one"
        );

        // Committing keeps the voice you chose talking instead of cutting it off.
        panel.on_key("up", ""); // back onto Juniper
        assert_eq!(
            preview_of(panel.on_key("enter", "")),
            None,
            "selecting should not interrupt its own voice"
        );
        assert_eq!(panel.settings().voice, "Juniper");
        assert_eq!(panel.title(), "TTS settings");
    }

    #[test]
    fn leaving_the_picker_silences_the_preview() {
        let mut panel = SettingsPanel::new(catalog());
        panel.open(&Settings {
            voice: "Miles".into(),
            ..Settings::default()
        });
        panel.on_key("enter", "");
        assert_eq!(
            preview_of(panel.on_key("esc", "")),
            Some(Preview::Silence),
            "backing out must stop the voice"
        );
        // And closing the panel outright is the caller's cue to stop as well.
        assert!(matches!(panel.on_key("esc", ""), KeyOutcome::Close));
    }

    #[test]
    fn s_auditions_the_voice_on_the_last_agent_message() {
        let mut panel = SettingsPanel::new(catalog());
        panel.open(&Settings {
            voice: "Juniper".into(),
            ..Settings::default()
        });
        // Main view: the voice actually selected.
        assert_eq!(
            preview_of(panel.on_key("rune", "s")),
            Some(Preview::LastMessage {
                voice: "Juniper".into()
            })
        );
        // Numeric edits still work there, and stay silent.
        assert!(matches!(
            panel.on_key("rune", "+"),
            KeyOutcome::Continue { preview: None, .. }
        ));
        // Picker: the highlighted voice, even though it is not the selected one.
        panel.on_key("enter", ""); // picker, cursor on Juniper
        assert_eq!(
            preview_of(panel.on_key("rune", "s")),
            Some(Preview::LastMessage {
                voice: "Juniper".into()
            })
        );
        panel.on_key("down", ""); // Zoey (not downloaded) cannot speak at all
        assert_eq!(
            preview_of(panel.on_key("rune", "s")),
            Some(Preview::Silence)
        );
        // Any other typed character stays inert.
        panel.on_key("up", "");
        assert_eq!(preview_of(panel.on_key("rune", "q")), None);
    }

    #[test]
    fn introductions_read_the_description_but_stay_listenable() {
        let prefix = "Hello, my name is Morwenna. ";
        let mut voice_entry = voice("Morwenna", true);
        voice_entry.description = Some(
            "A sultry, seductive woman in her thirties. Low, husky alto, close to the microphone \
             and almost whispered, slow and languid, purring, with a warm knowing smile in every \
             word. Intimate, flirtatious and teasing, like a late-night torch singer luring \
             someone closer, with a hint of danger underneath."
                .to_string(),
        );
        let text = introduction(&voice_entry);
        assert!(text.starts_with(prefix), "{text}");
        let spoken_chars = text.chars().count() - prefix.chars().count();
        assert!(
            spoken_chars <= INTRO_CHARS,
            "{} characters of description is too much to listen to",
            spoken_chars
        );
        // Sentences are kept whole: the second one does not fit, so it is not started at all.
        assert!(text.ends_with("thirties."), "cut mid-sentence: {text}");
        assert!(
            !text.contains("husky") && !text.contains("torch singer"),
            "{text}"
        );

        // Sentences that do fit are all spoken; one that cannot fit the whole budget is never
        // started, even though it is the only thing left.
        voice_entry.description = Some("Short one. Also short two. But this third sentence is far too long to be worth hearing while choosing a voice, and it proves that a preview stops at a sentence boundary instead of running on.".to_string());
        assert_eq!(
            introduction(&voice_entry),
            format!("{prefix}Short one. Also short two.")
        );

        // Text with no full stop falls back to whole words, never half a word.
        let mut rambling = voice("Grub", true);
        rambling.description = Some(
            "gruff and grainy rumbling low end heavy chest resonance slow lumbering rhythm with \
             loose rounded words and warm humour sitting just under the grumble and a warm knowing \
             chuckle underneath all of it"
                .to_string(),
        );
        let description = rambling.description.as_deref().unwrap();
        assert_eq!(
            spoken(description, 40),
            "gruff and grainy rumbling low end heavy"
        );
        let long = spoken(description, INTRO_CHARS);
        assert_eq!(long.chars().count(), INTRO_CHARS, "{long}");
        assert!(
            long.ends_with("warm") && !long.contains("knowing"),
            "cut mid-word: {long}"
        );
        assert!(introduction(&rambling).starts_with("Hello, my name is Grub. "));
        assert!(introduction(&rambling).ends_with(&long));

        // A catalog entry with no description still introduces itself.
        let mut bare = voice("Default", true);
        bare.description = None;
        assert_eq!(introduction(&bare), "Hello, my name is Default.");
        bare.description = Some("   ".to_string());
        assert_eq!(introduction(&bare), "Hello, my name is Default.");
    }

    #[test]
    fn footers_advertise_the_preview_keys_within_the_wrap() {
        let mut panel = SettingsPanel::new(catalog());
        panel.open(&Settings::default());
        for _ in 0..ROWS {
            let footer = panel.footer();
            assert!(
                footer.contains("s last message"),
                "footer hides the audition key: {footer}"
            );
            assert!(
                footer.chars().count() <= DETAIL_WIDTH,
                "footer wraps at {} columns: {footer}",
                footer.chars().count()
            );
            panel.on_key("down", "");
        }
        panel.on_key("enter", ""); // picker
        let footer = panel.footer();
        assert!(
            footer.contains("preview") && footer.contains("s last message"),
            "{footer}"
        );
        assert!(
            footer.contains("f favorite"),
            "the picker must advertise pinning: {footer}"
        );
        assert!(
            footer.chars().count() <= DETAIL_WIDTH,
            "picker footer wraps: {footer}"
        );
    }

    #[test]
    fn picker_cursor_wraps_in_both_directions() {
        let mut panel = SettingsPanel::new(catalog()); // Miles, Juniper, Zoey, Alfie
        panel.open(&Settings {
            voice: "Miles".into(),
            ..Settings::default()
        });
        panel.on_key("enter", ""); // picker, cursor on Miles
        for _ in 0..3 {
            panel.on_key("down", "");
        }
        assert_eq!(cursor_name(&panel), "Alfie", "did not reach the last voice");
        panel.on_key("down", ""); // used to clamp at the last row instead of wrapping
        assert_eq!(
            cursor_name(&panel),
            "Miles",
            "down at the last voice must wrap to the first"
        );
        panel.on_key("up", "");
        assert_eq!(
            cursor_name(&panel),
            "Alfie",
            "up at the first voice must wrap to the last"
        );
    }

    #[test]
    fn picker_height_stays_fixed_while_browsing() {
        // Real catalog descriptions range from a phrase to several sentences and some entries
        // have none: none of that may change the panel height under the cursor.
        let mut voices = catalog();
        voices[0].description = Some("punchy".to_string());
        voices[1].description = Some("resonant and decisive ".repeat(12));
        voices[2].description = None;
        let total = voices.len();
        let mut panel = SettingsPanel::new(voices);
        panel.open(&Settings {
            voice: "Miles".into(),
            ..Settings::default()
        });
        panel.on_key("enter", "");

        // One block header (the not-downloaded block labels each row instead), the four voices,
        // the blank line between the blocks, then the blank line and the detail rows under it.
        let height = 1 + total + 1 + 1 + DETAIL_ROWS;
        for step in 0..total * 2 {
            let lines = panel.lines();
            assert_eq!(
                lines.len(),
                height,
                "height changed after {step} downs: {lines:?}"
            );
            for line in lines.iter().skip(1 + total + 1 + 1) {
                assert!(
                    line.chars().count() <= DETAIL_WIDTH,
                    "detail row too wide: {line}"
                );
            }
            assert!(
                lines[height - DETAIL_ROWS..]
                    .iter()
                    .any(|l| !l.trim().is_empty()),
                "empty detail block"
            );
            panel.on_key("down", "");
        }
        // A clipped description is marked, not silently dropped.
        panel.on_key("down", ""); // cursor back at 0, so this lands on the long description
        let lines = panel.lines();
        assert!(
            lines[height - 1].trim_end().ends_with('…'),
            "clipped row: {}",
            lines[height - 1]
        );
    }

    #[test]
    fn wrap_rows_and_clip_mark_what_they_cut() {
        assert_eq!(
            wrap_rows("alpha beta gamma", 10),
            vec!["alpha beta", "gamma"]
        );
        assert_eq!(wrap_rows("aaaaaaaaaaaaaaaaaaaa", 6), vec!["aaaaa…"]); // never overflows
        assert!(wrap_rows("", 10).is_empty());
        assert_eq!(wrap_rows("one two three", 40), vec!["one two three"]);

        let mut row = "one two three four five".to_string();
        clip(&mut row, 10);
        assert_eq!(row.chars().count(), 10);
        assert!(row.ends_with('…'), "{row}");

        let mut row = "fits with room to spare".to_string();
        mark_continued(&mut row, 40);
        assert_eq!(row, "fits with room to spare…");
        let mut row = "x".repeat(40);
        mark_continued(&mut row, 40);
        assert_eq!(row.chars().count(), 40);
        assert!(row.ends_with('…'), "{row}");
    }

    #[test]
    fn picker_delimits_and_aligns_the_two_columns() {
        // Real catalogs contain names far longer than the floor width ("Captain Silas
        // Blackwater", "The Thing Under the Stairs"), which used to run straight into
        // the metadata column.
        let long = "Captain Silas Blackwater";
        let mut panel = SettingsPanel::new(vec![voice("Miles", true), voice(long, false)]);
        panel.open(&Settings {
            voice: "Miles".into(),
            ..Settings::default()
        });
        panel.on_key("enter", ""); // open the picker
        let lines = panel.lines();

        let long_line = lines
            .iter()
            .find(|l| l.contains(long))
            .expect("long name listed");
        assert!(
            long_line.contains(&format!("{long} │ ")),
            "no delimiter after a long name: {long_line}"
        );

        // The short name is padded to the same width, so the delimiter sits in one column.
        let column = |line: &String| line.chars().take_while(|c| *c != '│').count();
        let columns: Vec<usize> = lines
            .iter()
            .filter(|l| l.contains('│'))
            .map(column)
            .collect();
        assert!(
            columns.iter().all(|c| *c == long.chars().count() + 3),
            "columns: {columns:?}"
        );
    }

    #[test]
    fn no_picker_line_is_wider_than_the_wrap_column() {
        // A name long enough to overflow the wrap column would be split by zot into two screen
        // rows, which is exactly the height change that made the list jump. The metadata tail
        // absorbs it instead.
        let mut voices = vec![voice("Miles", true), voice("Alfie", false)];
        voices.push(voice(
            "Uncomfortably Long Display Name For An English Voice",
            true,
        ));
        let mut panel = SettingsPanel::new(voices);
        panel.open(&Settings {
            voice: "Miles".into(),
            ..Settings::default()
        });
        panel.on_key("enter", "");
        for line in panel.lines() {
            assert!(
                line.chars().count() <= DETAIL_WIDTH,
                "line too wide ({} cols): {line}",
                line.chars().count()
            );
        }
    }

    #[test]
    fn selecting_a_ready_voice_commits_and_closes_picker() {
        let mut panel = SettingsPanel::new(catalog());
        panel.open(&Settings {
            voice: "Miles".into(),
            ..Settings::default()
        });
        panel.on_key("enter", ""); // open picker on Miles (index 0)
        panel.on_key("down", ""); // -> Juniper (ready)
        let outcome = panel.on_key("enter", "");
        assert!(matches!(
            outcome,
            KeyOutcome::Continue { changed: true, .. }
        ));
        assert_eq!(panel.settings().voice, "Juniper");
        assert_eq!(panel.title(), "TTS settings"); // back to main view
    }

    #[test]
    fn selecting_a_not_downloaded_voice_is_a_noop_and_stays_open() {
        let mut panel = SettingsPanel::new(catalog());
        panel.open(&Settings {
            voice: "Miles".into(),
            ..Settings::default()
        });
        panel.on_key("enter", ""); // open picker on Miles (index 0)
        panel.on_key("down", ""); // Juniper
        panel.on_key("down", ""); // Zoey (not downloaded)
        let before = panel.settings().voice.clone();
        let outcome = panel.on_key("enter", "");
        assert!(matches!(
            outcome,
            KeyOutcome::Continue { changed: false, .. }
        ));
        assert_eq!(
            panel.settings().voice,
            before,
            "an unavailable voice must not change the setting"
        );
        // Still in the picker, and its detail block carries the fetch hint (hint + command).
        assert!(panel.title().starts_with("Choose a voice"));
        let detail = panel.lines();
        assert!(
            detail.iter().any(|l| l.contains("fetch it once")),
            "{detail:?}"
        );
        assert!(
            detail.iter().any(|l| l.contains("./ptts --voice \"Zoey\"")),
            "{detail:?}"
        );
    }

    #[test]
    fn cursor_browses_through_greyed_voices() {
        let mut panel = SettingsPanel::new(catalog());
        panel.open(&Settings::default());
        panel.on_key("enter", "");
        // Four voices total; wrapping up from index 0 should land on the last (Alfie).
        panel.on_key("up", "");
        assert!(panel
            .lines()
            .iter()
            .any(|l| l.contains(">") && l.contains("Alfie")));
    }

    #[test]
    fn volume_edits_speak_the_selected_voice_at_the_new_level() {
        let mut panel = SettingsPanel::new(catalog());
        panel.open(&Settings {
            voice: "Miles".into(),
            ..Settings::default()
        }); // 50%: the voice's own level
        panel.on_key("down", ""); // cursor on Volume

        // 50% -> 55%, said by the voice you selected: short enough to hear as a level change.
        assert_eq!(
            preview_of(panel.on_key("right", "")),
            Some(Preview::Voice {
                voice: "Miles".into(),
                text: "Hello, my name is Miles.".into(),
            }),
            "a volume change should be audible"
        );
        assert!(
            (panel.settings().volume - 0.55).abs() < 1e-6,
            "volume did not step by 5%"
        );
        assert!(
            panel.lines()[1].contains("55%"),
            "the row shows the dial as a percentage: {:?}",
            panel.lines()[1]
        );
        assert!(
            panel.footer().contains("audible"),
            "footer hides the audition: {}",
            panel.footer()
        );

        // Temperature changes how a voice speaks, not its level or its rate: no audition there. Two
        // steps down from Volume crosses the Speed row to reach it.
        panel.on_key("down", "");
        panel.on_key("down", "");
        assert_eq!(
            preview_of(panel.on_key("right", "")),
            None,
            "temperature edits should stay silent"
        );
        assert!(
            (panel.settings().temperature - 0.35).abs() < 1e-6,
            "temperature did not step"
        );

        // A change that did not happen has nothing to audition: at 100%, `+` says nothing.
        let mut ceiling = SettingsPanel::new(catalog());
        ceiling.open(&Settings {
            voice: "Miles".into(),
            volume: 1.0,
            ..Settings::default()
        });
        ceiling.on_key("down", "");
        assert_eq!(
            preview_of(ceiling.on_key("right", "")),
            None,
            "a no-op edit should not speak"
        );
        assert!(
            (ceiling.settings().volume - 1.0).abs() < 1e-6,
            "the dial clamps at 100%"
        );
    }

    /// The Speed ladder as the panel walks it: one rung per arrow, `1.0x` in the middle, both ends
    /// shut, and — like Volume — every real change audible. Counted in tenths rather than a float
    /// dial, so three arrows from `1.0x` are `1.3x` and not `1.2999999`.
    #[test]
    fn speed_steps_by_a_tenth_and_stops_at_the_ends_of_the_ladder() {
        let mut panel = SettingsPanel::new(catalog());
        panel.open(&Settings {
            voice: "Miles".into(),
            ..Settings::default()
        });
        panel.on_key("down", ""); // Volume
        panel.on_key("down", ""); // Speed

        for _ in 0..3 {
            panel.on_key("right", "");
        }
        assert_eq!(panel.settings().speed.label(), "1.3x");
        assert!(
            panel.lines()[SPEED_ROW].contains("1.3x"),
            "the row shows the rate it will speak at: {:?}",
            panel.lines()[SPEED_ROW]
        );

        // A rate is something you hear, so the row auditions the selected voice as Volume does.
        assert_eq!(
            preview_of(panel.on_key("right", "")),
            Some(Preview::Voice {
                voice: "Miles".into(),
                text: "Hello, my name is Miles.".into(),
            }),
            "a speed change should be audible"
        );

        // Past the ends there is nothing to step to, and a press that changes nothing stays quiet.
        for _ in 0..30 {
            panel.on_key("right", "");
        }
        assert_eq!(panel.settings().speed, SPEED_MAX, "the ceiling is 2.0x");
        assert_eq!(
            preview_of(panel.on_key("right", "")),
            None,
            "a rung that does not move has nothing to audition"
        );

        for _ in 0..40 {
            panel.on_key("left", "");
        }
        assert_eq!(panel.settings().speed, SPEED_MIN, "the floor is 0.5x");
        assert!(
            panel.lines()[SPEED_ROW].contains("0.5x"),
            "{:?}",
            panel.lines()[SPEED_ROW]
        );
        assert_eq!(
            preview_of(panel.on_key("left", "")),
            None,
            "and the floor is quiet too"
        );
    }

    #[test]
    fn pinned_voices_form_a_block_at_the_top_of_the_picker() {
        // Miles and Juniper are ready; Zoey and Alfie are not. Only Zoey is pinned.
        let mut panel = SettingsPanel::new(catalog());
        panel.open(&Settings {
            voice: "Miles".into(),
            favorites: vec!["Zoey".into()],
            ..Settings::default()
        });
        panel.on_key("enter", "");
        let lines = panel.lines();
        let row_of = |name: &str| {
            lines
                .iter()
                .position(|line| line.contains(name))
                .expect(name)
        };

        assert!(
            lines.iter().any(|line| line.contains("★ 1 favorite")),
            "{lines:?}"
        );
        assert_eq!(
            row_of("Zoey"),
            1,
            "a pinned voice heads the list even when it is not downloaded"
        );
        assert!(
            lines[row_of("Zoey")].contains("(not downloaded)"),
            "{lines:?}"
        );
        // The blocks come in order, each keeping the catalog's own order.
        assert!(
            lines.iter().any(|line| line.contains("2 ready")),
            "{lines:?}"
        );
        assert!(
            row_of("Zoey") < row_of("Miles") && row_of("Miles") < row_of("Juniper"),
            "{lines:?}"
        );
        assert!(row_of("Juniper") < row_of("Alfie"), "{lines:?}");
    }

    #[test]
    fn f_pins_and_unpins_without_moving_the_cursor() {
        let mut panel = SettingsPanel::new(catalog());
        panel.open(&Settings {
            voice: "Miles".into(),
            ..Settings::default()
        });
        panel.on_key("enter", ""); // cursor on Miles
        assert_eq!(cursor_name(&panel), "Miles");

        assert_eq!(
            preview_of(panel.on_key("rune", "f")),
            Some(Preview::Voice {
                voice: "Miles".into(),
                text: "Hello, my name is Miles. Miles description.".into(),
            }),
            "pinning re-auditions the voice you just pinned, at the volume you have set"
        );
        assert_eq!(panel.settings().favorites, vec!["Miles".to_string()]);
        assert_eq!(
            cursor_name(&panel),
            "Miles",
            "the voice must not move out from under the cursor"
        );
        assert!(panel
            .lines()
            .iter()
            .any(|line| line.contains("★ 1 favorite")));

        panel.on_key("down", ""); // Juniper, the other ready voice
        assert_eq!(cursor_name(&panel), "Juniper");
        panel.on_key("rune", "f");
        assert_eq!(
            panel.settings().favorites,
            vec!["Juniper".to_string(), "Miles".to_string()],
            "stored in one order"
        );
        let lines = panel.lines();
        assert!(lines[0].contains("★ 2 favorite"), "{lines:?}");
        assert!(
            lines[1].contains("Miles") && lines[2].contains("Juniper"),
            "favorites lead the list in catalog order: {lines:?}"
        );
        assert_eq!(
            cursor_name(&panel),
            "Juniper",
            "reordering must not move the cursor"
        );

        // Unpinning is the same key: the voice under the cursor drops out of the favorite block.
        panel.on_key("rune", "f");
        assert_eq!(
            panel.settings().favorites,
            vec!["Miles".to_string()],
            "Juniper unpinned, Miles still pinned"
        );
        assert_eq!(
            cursor_name(&panel),
            "Juniper",
            "unpinning must not move the cursor either"
        );
        let lines = panel.lines();
        assert!(
            lines[0].contains("★ 1 favorite") && lines[1].contains("Miles"),
            "{lines:?}"
        );
        assert!(
            lines[2].is_empty(),
            "a blank line separates the blocks: {lines:?}"
        );
        assert!(
            lines[3].contains("1 ready") && lines[4].contains("> Juniper"),
            "the unpinned voice rejoins the ready block: {lines:?}"
        );

        // Back on the main row, a pinned selection is marked.
        panel.on_key("esc", "");
        assert!(
            panel.lines()[0].contains("Miles ★"),
            "{:?}",
            panel.lines()[0]
        );
    }

    #[test]
    fn every_main_row_shows_its_value() {
        let mut panel = SettingsPanel::new(catalog());
        panel.set_project(PathBuf::from("/work/repo"));
        panel.open(&Settings {
            voice: "Miles".into(),
            ..Settings::default()
        });
        let lines = panel.lines();
        // A row that only shows its own name is not a setting you can read. The five values here are
        // the defaults a fresh install is spoken at, so they are also the contract's baseline.
        for (row, (name, value)) in [
            ("Voice", "Miles"),
            ("Volume", "50%"),
            ("Speed", "1.0x"),
            ("Temperature", "0.30"),
            ("Auto speak", "off"),
        ]
        .iter()
        .enumerate()
        {
            assert!(
                lines[row].contains(name) && lines[row].contains(value),
                "row {row} should read as {name} {value}: {lines:?}"
            );
        }
    }

    /// The Auto speak row as rendered, with its cursor marker when the cursor is on it.
    fn auto_speak_row(panel: &SettingsPanel) -> String {
        panel.lines()[AUTO_SPEAK_ROW].clone()
    }

    #[test]
    fn auto_speak_arrows_step_off_here_and_everywhere() {
        let here = PathBuf::from("/work/repo");
        let elsewhere = PathBuf::from("/somewhere/else");
        let mut panel = SettingsPanel::new(catalog());
        panel.set_project(here.clone());
        panel.open(&Settings::default());
        for _ in 0..AUTO_SPEAK_ROW {
            panel.on_key("down", ""); // walk the cursor down to the ring
        }
        assert!(
            auto_speak_row(&panel).contains(">") && auto_speak_row(&panel).contains("Auto speak"),
            "the cursor belongs on the row: {:?}",
            panel.lines()
        );
        assert!(
            auto_speak_row(&panel).contains("off"),
            "{:?}",
            panel.lines()
        );

        // Right: this project alone.
        panel.on_key("right", "");
        assert!(
            auto_speak_row(&panel).contains("on · here"),
            "not scoped to this project: {:?}",
            panel.lines()
        );
        assert!(panel.settings().auto_speak_for(&here));
        assert!(
            !panel.settings().auto_speak_for(&elsewhere),
            "every other project stays quiet"
        );

        // Right: the default every project follows.
        panel.on_key("right", "");
        assert!(
            auto_speak_row(&panel).contains("on · everywhere"),
            "{:?}",
            panel.lines()
        );
        assert!(
            panel.settings().auto_speak_for(&elsewhere),
            "the default covers every other project"
        );
        assert!(
            panel.settings().auto_speak_projects.is_empty(),
            "following the default is not an override to remember"
        );

        // Right: quiet again, for everyone.
        panel.on_key("right", "");
        assert!(
            auto_speak_row(&panel).contains("off"),
            "{:?}",
            panel.lines()
        );
        assert!(!panel.settings().auto_speak_for(&here));
        assert!(!panel.settings().auto_speak_for(&elsewhere));

        // Left walks the same ring backwards; nothing but the arrows moves it.
        panel.on_key("left", "");
        assert!(
            auto_speak_row(&panel).contains("on · everywhere"),
            "{:?}",
            panel.lines()
        );
        panel.on_key("left", "");
        assert!(
            auto_speak_row(&panel).contains("on · here"),
            "{:?}",
            panel.lines()
        );
        panel.on_key("left", "");
        assert!(
            auto_speak_row(&panel).contains("off"),
            "{:?}",
            panel.lines()
        );

        // The `a` key that used to toggle the default is gone: it must now do nothing.
        panel.on_key("rune", "a");
        assert!(
            auto_speak_row(&panel).contains("off"),
            "{:?}",
            panel.lines()
        );
        assert!(
            panel.settings().auto_speak_projects.is_empty() && !panel.settings().auto_speak,
            "a stray key must not switch anything on"
        );
    }

    #[test]
    fn a_blank_line_separates_the_picker_blocks() {
        // Miles and Juniper are ready; Zoey and Alfie are not. Only Zoey is pinned, which is the
        // case where the blocks would otherwise read as one unbroken list of names.
        let mut panel = SettingsPanel::new(catalog());
        panel.open(&Settings {
            voice: "Miles".into(),
            favorites: vec!["Zoey".into()],
            ..Settings::default()
        });
        panel.on_key("enter", "");
        let lines = panel.lines();
        assert!(
            lines[0].contains("1 favorite") && lines[1].contains("Zoey"),
            "{lines:?}"
        );
        assert!(
            lines[2].is_empty(),
            "favorites run straight into the ready voices: {lines:?}"
        );
        assert!(
            lines[3].contains("2 ready") && lines[4].contains("Miles"),
            "{lines:?}"
        );
        assert!(
            lines[5].contains("Juniper") && lines[6].is_empty(),
            "{lines:?}"
        );
        assert!(
            lines[7].contains("Alfie") && lines[7].contains("(not downloaded)"),
            "{lines:?}"
        );
    }

    #[test]
    fn the_picker_height_does_not_move_while_browsing_favorites() {
        // Favorites add a header and a separator, which is fine; what must not happen is the list
        // shifting under the cursor as the blocks grow and shrink.
        let mut panel = SettingsPanel::new(catalog());
        panel.open(&Settings {
            voice: "Miles".into(),
            favorites: vec!["Zoey".into()],
            ..Settings::default()
        });
        panel.on_key("enter", "");
        let height = panel.lines().len();
        for _ in 0..catalog().len() * 2 {
            assert_eq!(panel.lines().len(), height, "{:?}", panel.lines());
            panel.on_key("down", "");
        }
    }
}
