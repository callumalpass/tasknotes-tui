# Changelog

## Unreleased

- fix recurring tasks being rescheduled a day early in timezones ahead of UTC when the rule has a date-only or floating `DTSTART` (#8; diagnosis and fix by @kehao-chen)

## v0.1.3 - 2026-03-31

- add active project context, including `Shift-P` toggle behavior and project-aware quick create
- add a default Project view and project-scoped expression variables
- resolve task `projects` links for project filtering, including wikilink and markdown-link cases
- preserve focused date when switching back to the Date view
- surface the active project in the State pane
- refresh the README to document the current defaults and project workflow
