# Working on heyl

## Changelog

Before every commit, summarize the change as one short list entry (1-4 lines, written for a
user of heyl, not for its developers) and add it at the **top** of the list in `CHANGELOG.md`,
directly under the `# Changelog` title, as part of the same commit.

Do not add `## Release ...` headings yourself: the nightly release workflow
(`.github/workflows/release.yml`) inserts one above the pending entries when it cuts a release,
and uses them as that release's notes.
