//! What reaches the phonemizer, before and after `zot_speaks::tts::spoken_text`.
//!
//! Run it with `cargo run --release --example spoken_text_probe`. It loads no model: both columns are
//! pure text, `ptts`' own normalization applied to the message as written and to what this extension
//! hands it. It is the probe behind the claims in `docs/` and `CHANGELOG.md` about what a voice cannot
//! read, and it is why the filter drops fences, tables and glossed code rather than braces (which
//! `ptts` already drops).
use ptts::preprocess::{normalize_text, Lang, Rules};
use zot_speaks::tts::spoken_text;

fn main() {
    let cases = [
        // The house shape (ADR-0039): the words carry the sentence, the exact form is for the eye.
        "Run the formatter over all targets with the provided command `cargo fmt --all -- --check`.",
        "The GitHub command line tool (`gh`) bullet carries the reading.",
        "The working notes (`AGENTS.md`) carry the reading, and the words are the subject.",
        "The harness (`dev/test.sh`) and the sink test (`dev/test-sink.sh`) cover it.",
        "The tag is version one ten one (`v1.10.1`).",
        "```rust\nlet x = foo(bar);\n```\nDone.",
        "| card | state |\n|---|---|\n| v1.10.1 | done |\nThree cards are in ready.",
        "**bold** and # heading and - bullet and [link](docs/DESIGN.md)",
        "## The gates\n### One commit carries the change\nAll green.",
        "## Run the formatter (`cargo fmt`)\nA heading whose words end in a parenthesized name.",
        "## Is it fast?\n## Listen:\n## Stop!\nQuiet.",
        // The case behind the skill's number rule: a figure with grouping spaces reaches the model as
        // digit groups, the same digits without them as a number, and a colon clock unchanged.
        "Checked at 16:52:06: 7 458 792 bytes, or 7458792, about 7.4 MB.",
        // A span is silent wherever it sits, so a figure that has to be heard is written as words and
        // the exact digits stay for the eye.
        "The manifest is about seven megabytes (`7458792`).",
    ];
    for case in cases {
        let spoken = spoken_text(case);
        println!(
            "written   : {}\nbefore    : {}\nafter     : {}\n",
            case.replace('\n', "\\n"),
            normalize_text(case, Lang::En, Rules::DEFAULT),
            normalize_text(&spoken, Lang::En, Rules::DEFAULT)
        );
    }
}
