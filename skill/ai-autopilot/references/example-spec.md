# Example Project Spec: Simple REST API

> This is an example spec document. Give your own spec to the ai-autopilot skill
> and it will generate a tailored prompt pipeline.

## Project Rules

These rules apply to every stage of development:

1. All code must be written in Rust (edition 2021).
2. Use axum for HTTP routing and tokio as the async runtime.
3. Every public function must have a doc comment.
4. Write at least one test per module.
5. Use `anyhow` for error handling throughout.
6. Follow the directory structure established in the Foundation stage.

## Overview

Build a simple REST API for managing a list of bookmarks. Users can create,
read, update, and delete bookmarks. Each bookmark has a URL, title, optional
description, and creation timestamp.

## Sections

### 1. Data Model

Define a `Bookmark` struct with:
- `id: i64` (auto-increment primary key)
- `url: String`
- `title: String`
- `description: Option<String>`
- `created_at: DateTime<Utc>`

### 2. Storage Layer

Use SQLite via sqlx. Provide functions:
- `create_bookmark(pool, url, title, desc) -> Result<Bookmark>`
- `list_bookmarks(pool) -> Result<Vec<Bookmark>>`
- `get_bookmark(pool, id) -> Result<Option<Bookmark>>`
- `update_bookmark(pool, id, url, title, desc) -> Result<Bookmark>`
- `delete_bookmark(pool, id) -> Result<bool>`

### 3. HTTP API

Expose these endpoints:
- `POST   /bookmarks`        — create
- `GET    /bookmarks`        — list all
- `GET    /bookmarks/:id`    — get one
- `PUT    /bookmarks/:id`    — update
- `DELETE /bookmarks/:id`    — delete

Return JSON. Use appropriate HTTP status codes (201, 200, 404, 400).

### 4. Error Handling

Map storage errors to HTTP responses:
- Not found → 404
- Validation failures → 400 with a JSON error body
- Internal errors → 500

### 5. Configuration

Read from environment variables:
- `DATABASE_URL` (default: `sqlite://bookmarks.db`)
- `PORT` (default: `3000`)

## Testing Requirements

- Unit tests for the storage layer using an in-memory SQLite database.
- Integration tests that start the full axum server and send HTTP requests.
- Test all CRUD paths including error cases (missing ID, invalid URL).
- `cargo test` must pass with zero failures.
