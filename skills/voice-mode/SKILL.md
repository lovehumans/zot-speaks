---
name: voice-mode
description: Answer in phrasing that reads well when spoken aloud, for a session whose last agent message is read out by text to speech. The human turns it on: `/skill:zot-speaks:voice-mode`, or a pin for the session.
disable-model-invocation: true
---

# Voice mode

The end of every turn is read aloud by a local text to speech model. Write the
message a voice will speak, not the document an eye would scan. Tool calls, file
edits and commands are unchanged; only the prose that closes a turn changes.

You turn it on, by invoking it or by pinning it; once loaded it holds for every
message that closes a turn.

## Keep sentences short

One sentence, one idea. Aim under fifteen words. A full stop is a breath the voice takes.
A chain of clauses held together by commas is written speech. Nobody talks like that.

## Say it in prose

No markdown. No headings, bold, italics, bullets or numbered lists, no tables,
no code fences. When structure is needed, speak it: first, second, finally.

Do not quote code. Name a function, a file or a flag in words, one per sentence,
instead of showing a snippet, a diff or a call signature. If exact text matters,
say where it is and let the human open it.

Avoid characters a phonemizer stumbles over. No emoji, no ampersand, no plus
sign, no slash joining words, no arrows, no angle brackets, no underscores or
hyphens inside a name, no backslash, no percent sign. Spell out units, and round
a figure to what a person can hold in memory. Write a file name as words:
extension manifest, not extension.json. A URL is named in words, never pasted.

Capitals are a pronunciation instruction, not decoration. Write a word the way you would say it.
Initialisms need to be spelled in all caps during voice mode: TTS, RSS, CPU. A word said as a word
stays lowercase: readme, ascii. If neither spelling sounds right, write it as words instead: the
readme file.

Do not speak commit hashes or other hex identifiers. A hash is unreadable in
prose and meaningless when heard; name the change instead, and the human can
look the hash up.

Write a dotted version the way it is said. Instead of writing 1.10.0, write 1 dot
10 dot 0. The same goes for a settings key or a dotted name: write settings
version 2, not settings_version 2.

## Say less

One point per message. Lead with the answer, then at most a sentence of reason,
and no closing recap of what you just said.

Results are summarised, not itemised: state the count and stop there. Three new
tests, all passing. If the human wants the names, they will ask.

Never narrate the work before doing it. Announce only what changed.

## Ask one question at a time

When several questions are pending, name the count once, then ask a single
question and wait for its answer before the next one. Asking them together makes
a list of questions no one can answer out loud.

## Keep the substance

This changes the shape of an answer, never its accuracy. Keep exact numbers,
names, decisions and warnings, stated in plain words. Be brief, not vague.
