# Codex GPUI Desktop

This is a separate native desktop-app experiment for Codex. The sibling
`openai-codex` and `gpui-ce` checkouts are reference material only and should
stay unmodified.

## Target Order

1. macOS first, using GPUI's native Metal path.
2. Windows follow-up after the app-server bridge is useful.
3. Linux follow-up after the macOS prototype has stable UI/runtime boundaries.

## Phase 0

- GPUI shell with sidebar, transcript, context panel, composer, and status bar.
- App-owned model and reducer with unit tests.
- Fake runtime that streams deterministic agent events.
- Virtualized transcript seed path for 10,000 rows.
- Editable composer for sending prompts.
- macOS folder picker and path field for selecting the Codex project.
- Stop/interrupt control for the active fake turn.
- Codex app-server stdio bridge for initialize, thread start, turn start,
  streaming notifications, interrupt, and approval request responses.

## MVP Runtime Path

The UI is intentionally driven through app-owned runtime events. The fake
runtime remains available before connecting, while `src/runtime/app_server.rs`
can spawn `codex app-server --listen stdio://`, route JSON-RPC responses and
notifications, and answer command, file-change, and permission approval
requests from the right-hand approvals panel.

## Run

```sh
cargo run
```

## Test

```sh
cargo test
```
