# Wiki source fixtures

`wiki.json` contains excerpts from the Dandy’s World Wiki contributors on Fandom,
licensed under [CC BY-SA 3.0](https://creativecommons.org/licenses/by-sa/3.0/).
The excerpt collection and adaptations of its text are distributed under that
same license. This notice applies to source text, not the original parser code.

Each record credits its original article with `source_url`, title, revision link,
revision timestamp, and retrieval time. Those article links provide contributor
history. See [Fandom’s attribution policy](https://www.fandom.com/licensing).

Changes: selected infoboxes, sections, a table row, and supporting templates were
extracted from the local revision snapshot and packaged as JSON test fixtures.
`selection.start/end` are character offsets into the original source, and both
full-source and excerpt SHA-256 values are retained. No artwork or audio is
included. Generated summaries retain these source/attribution fields.

These records test parser behavior against particular wiki revisions; they are
not a claim about current game balance or a complete wiki dataset.
