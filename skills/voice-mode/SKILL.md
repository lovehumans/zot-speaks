---
name: voice-mode
description: Answer in phrasing that reads well when spoken aloud. Code and tables may stay in the message, but a voice does not read them, so anything that has to be heard is written as words, with the exact form in parentheses after them. The human turns it on with `/skill:zot-speaks:voice-mode` or a pin in `/skills`, and the model may invoke it too when the answer is meant to be heard.
disable-model-invocation: false
---

# Voice mode

The end of every turn is read aloud by a local text to speech model. Write the
message a voice will speak, not the document an eye would scan. Tool calls, file
edits and commands are unchanged; only the prose that closes a turn changes.

The skill is loaded when you invoke it or pin it in `/skills`, and the model may invoke it too when the
answer is meant to be heard. Once loaded it holds for every message that closes a turn.

## Keep sentences short

One sentence, one idea. Aim under fifteen words. A full stop is a breath the voice takes.
A chain of clauses held together by commas is written speech. Nobody talks like that.

## Code and tables stay for the eye

A voice cannot read code, so the extension takes it out of what it speaks. A
fenced block is dropped whole, and so is every row of a markdown table and the
`#` marker of a heading, which gains a full stop so the voice breathes after
it. Every inline code span is dropped too, alone or inside parentheses: nothing
that only appears in code is ever heard.

Write the code you would write anyway, and put whatever has to be heard in the
words around it: say what a thing is, then the exact form in parentheses after
it. "the formatter over all targets (`cargo fmt --all -- --check`)" is heard as
"the formatter over all targets", and the command stays on the screen. Words
first, exact form second, because the model reads in the order you wrote it. An
article with no noun ahead of a parenthesized form is stranded: "the
(`dev/test.sh`) runs the units" is heard as the bare word "the". A pair of
parentheses holding no words is dropped as well, being what a deleted span
leaves behind; for a breath, write a full stop. Those words are also the subject
a voice hears, so the verb agrees with them: "the working notes (`AGENTS.md`)
carry", not "carries".

A table is silent, so follow it with the one sentence that carries its point.
When structure belongs in the speech, speak it: first, second, finally.

## Write prose a phonemizer handles

In the prose around your code, avoid characters a phonemizer stumbles over. No
emoji, no ampersand, no plus sign, no arrows, no angle brackets, no percent
sign, no hashtag. Spell out units, and round a figure to what a person can hold in memory.
A URL is named in words, never pasted.

Capitals are a pronunciation instruction, not decoration. Write a word the way you would say it.
Initialisms need to be spelled in all caps during voice mode: TTS, RSS, CPU. A word said as a word
stays lowercase: readme, ascii. If neither spelling sounds right, write it as words instead: the
readme file.

A version is said by its numbers, never by the word for the dot. Write them in
the sentence and keep the code spelling for the eye, since a span is silent: "the
release is version one eleven zero (`v1.11.0`)", not "v1.11.0" in prose, a bare
"`v1.11.0`", or "`v1.11.0`(version one dot eleven dot zero)".
The same goes for a settings key: write "settings version 2", not "settings_version 2".

Never voice an identifier. A commit hash, a Card name, a ticket id, a long
branch or file name: whatever tool made it, a voice cannot read it, so never
write one bare in prose. Name the thing in words and keep its exact form in
code, so the eye finds it and the voice hears its reason: the release commit
(`5dd559e`), the Card about the phonemization
(`2026-11-23T1727-improving-code-phonemization`). Those words are what a voice
says, so they have to stand on their own, and a command that needs the exact
identifier belongs in a fenced block, which is silent.

## Numbers: state the finding, not the figure

A number written for a machine is written for the eye, and a voice reads it as
digits. Measured in `cargo run --release --example spoken_text_probe`:
`7 458 792 bytes` reaches the model as "seven, four five eight, seven nine two
bytes", the same digits as `7458792 bytes` reach it as "7 million 458 thousand
792 bytes", and `16:52:06` arrives unchanged.

So rewrite the sentence until the figure is no longer what it says. "I checked
the timestamp and the size of the running install, and it carries the new rule"
is the sentence; the counts stay in a table, a log line, or a fenced block,
where the eye reads them.

When a figure is the point, write it as you would say it: about seven megabytes,
two hundred milliseconds, just under forty cases, with no grouping spaces, no
colon clock and no unit you cannot pronounce. When the exact figure has to be
heard, write it out in words, and keep `7458792` in a table or a code span.

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
names, decisions and warnings, stated in plain words. Exact figures live in
code, in a table, or in a log line for the eye; the sentence a voice speaks says
what they show. Be brief, not vague.
