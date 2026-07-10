---
number: 182
title: Limit queued prompt autostart concurrency
status: done
priority: high
labels: [tui, config]
dependson: []
related: []
created_at: 2026-07-10T23:40:04.921565+00:00
updated_at: 2026-07-10T23:40:09.099396+00:00
---

## Purpose

Limit queued prompt autostart so prompts remain queued while agent concurrency is capped.

## Scope

- Add global/local settings for the autostart queued prompt agent limit.
- Count occupied slots from agent phases: running/waiting occupy; ended/ready/none are free.
- Start only the available number of queued sessions without taking prompts past the limit.
- Expose the setting in Config UI and docs.
- Cover limit behavior, phase counting, and config persistence with tests.

## Verification

- cargo fmt
- cargo clippy --all-targets -- -D warnings
- cargo test
