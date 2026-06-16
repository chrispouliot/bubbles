# Plan: Inline link previews (rich URL cards) in the message timeline

## Goal

When a message contains a URL, render a compact preview card directly under
(or attached to) the bubble instead of a bare clickable link. The card shows
the page's `og:title` / `og:description`, the site name, and the
`og:image` thumbnail. Clicking the card (or the URL) opens the original
link in the system browser. If we have no preview yet, show a
"Loading…" placeholder and fill it in once the fetch returns.

## Why this is the right place to add it

* The URL→preview pipeline runs **on the tokio runtime** and writes to the
  existing SQLite store. No new state has to live in the UI; the same
  `populate_messages` / `reload_messages` path that already reacts to DB
  changes picks the new card up for free.
* Preview state is keyed by URL, not by message, so the same link shared
  in five different chats is fetched and stored once. The fan-out dedup we
  already do on `message.guid` (see `store/mod.rs::insert_message`) means
  re-arrival of the same message doesn't re-fetch.
* Keeping previews in the DB means the next launch (or scroll-back through
  history) renders the card immediately from cache without going to the
  network — same model as iMessage/Signal/Telegram link previews.

## Current state (recap)

* `src/ui/mod.rs::bubble_label` already detects URLs and renders them as
  Pango `<a>` markup via `text_to_markup` (the clickable-links change
  just landed).
* `message_body` builds a `gtk::Box` column: attachments → bubble
  containing the text label.
* `populate_messages` (and `build_message_widgets`) iterate over
  `Vec<StoredMessage>` and append one `message_widget` per row into
  `msg_container`.
* Reloads happen through `Ui::reload_messages` (window-only, via
  `messages_from`) and `populate_messages` rebuilds the whole window.

## Design

### 1. Data model — new table `link_preview`

Add to the `DDL` block in `src/store/mod.rs`, with a migration (`migrate`):

```sql
CREATE TABLE link_preview(
  url          TEXT PRIMARY KEY,    -- normalized (lowercased scheme+host, no fragment)
  title        TEXT,
  description  TEXT,
  site_name    TEXT,
  image_path   TEXT,                -- local cached thumbnail (or NULL)
  fetched_at   INTEGER NOT NULL,    -- unix ms; used for TTL
  status       INTEGER NOT NULL,    -- 0 = ok, 1 = fetch failed, 2 = still pending
  error        TEXT                 -- short reason, e.g. "timeout", "no title"
);
CREATE INDEX idx_link_preview_fetched ON link_preview(fetched_at);
```

* `url` is the dedupe key. A second message sharing the same URL reuses
  the same row.
* `status` is what lets the UI distinguish "not tried yet" from "tried
  and failed" — without it, on a cold start every URL would look like
  "loading" again.
* `image_path` points into a new cache directory under
  `$XDG_CACHE_HOME/openbubbles-gtk/previews/<sha256(url)>` so the same
  thumbnail is reused across messages and the DB holds only the
  relative path. The store opens with that dir and exposes a helper
  `preview_image_path_for(url) -> PathBuf`.
* `fetched_at` carries a TTL (default 7 days) so a still-stale card can
  be silently refreshed in the background; cards older than the TTL
  also re-fetch the first time we render them.

Add `pub async fn get_link_preview(&self, url) -> Option<LinkPreview>`
and `pub async fn upsert_link_preview(&self, &LinkPreview)` to
`Store`. The async wrapper just calls a new sync `query_link_preview` /
`upsert_link_preview` on the connection, matching the existing pattern.

In `model.rs`, add:

```rust
pub struct LinkPreview {
    pub url: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub site_name: Option<String>,
    pub image_path: Option<String>,
    pub fetched_at: i64,
    pub status: i64,
    pub error: Option<String>,
}
```

### 2. Fetcher — new module `src/preview.rs`

A self-contained async module the tokio runtime can call from a
`tokio::task::spawn`. Responsibilities:

* **Normalize the URL.** Lowercase scheme + host, drop fragment, trim
  trailing punctuation, drop default ports (`:80`, `:443`),
  drop a trailing `?utm_*=…` set so tracking doesn't break dedupe.
  The same canonical form is the DB key.
* **HTML fetch (ureq).** `ureq` is already in `Cargo.lock` transitively
  (pulled by `ksni`'s config tooling, see `Cargo.lock`). Use it directly:
  * `GET` with a desktop `User-Agent` (stable, identifying the client
    only by name + version — no user identifiers).
  * 10 s connect timeout, 10 s read timeout.
  * `Accept: text/html,application/xhtml+xml` and a hard cap of
    **1 MiB** on the response body (drop the connection after).
  * Follow up to **3 redirects**; on the next redirect to a different
    host, follow it too. After that, stop.
  * Treat anything outside `2xx` as a failure (`status=1`).
  * Refuse to fetch non-HTTP(S) schemes and refuse to fetch hosts that
    resolve to private/loopback/link-local IPs — small SSRF guard.
    Cheap implementation: resolve with `std::net::ToSocketAddrs` once
    on first hit and remember the verdict in a process-local `HashSet`;
    bail if every address is in a private range.
* **Parse `<meta>` and OG tags.** Hand-rolled — the OG spec is a few
  dozen tags and a real HTML parser is a heavy dep. Use one `regex` to
  pull every `<meta …>` and `<title>…</title>`, then look up by
  `property` / `name`:
  * `og:title`  → title
  * `og:description` (or `description` / `twitter:description`)
  * `og:site_name` (or derive from host)
  * `og:image` / `og:image:url` / `twitter:image` → image URL
  * `<title>…</title>` as a fallback
  All values are HTML-entity-decoded (a small `decode_entities` fn
  covering `&amp; &lt; &gt; &quot; &#39;` and `&#NNN;` / `&#xHH;`).
  Truncate `title` to 200 chars and `description` to 400 chars.
* **Image download.** If we have an image URL and it resolves
  relative to the page (e.g. `/og.png`), resolve to absolute. Same
  fetch caps as the page, plus: only accept `image/*` content-types
  and only if the body is ≤ 4 MiB. Save to
  `$XDG_CACHE_HOME/openbubbles-gtk/previews/<sha256(image_url)>` and
  record the relative path in `image_path`. If the image fails, set
  `image_path = NULL` but keep the text fields (so we still show a
  card without a thumbnail).
* **Result.** On success, `LinkPreview { status: 0, … }`. On any
  failure, `status: 1` with `error: "timeout" | "no title" | "private
  host" | …`. A row with `status: 1` is still cached — re-trying on
  every render would hammer sites that 404'd once.

Add `pub async fn fetch(url: String) -> LinkPreview` and a single
shared `pub static IN_FLIGHT: Lazy<Mutex<HashMap<String, …>>>` that
keys on the normalized URL. Two renders of the same URL at the same
time share one in-flight task instead of racing.

### 3. Wiring it into the render path

`message_body` currently loops `m.attachments` then adds a text
bubble. Extend it so that, **after** the text label, it appends a
`link_preview_card(m, url)` widget per URL found in `body_text(m)`.
The URL is the only thing we extract at render time — re-using the
existing `URL_RE` (in `src/ui/mod.rs`) keeps regex construction cheap
(`OnceLock<Regex>`), and we run it on `body_text(m)`, not the raw
text, so the U+FFFC strip happens first.

```text
message_body(m, own)
├── for each attachment: pic | file_chip
├── bubble
│   └── bubble_label(body_text(m))   // existing clickable links
└── for each url in body_text(m):
    └── link_preview_card(m, url)    // new
```

`link_preview_card(url)` is a thin wrapper:

1. Synchronously call `Store::get_link_preview(url)`. We are on the
   GTK main thread, but the store is async; use the existing
   `gtk_bridge::spawn` shape: fire-and-forget with an `on_done` that
   updates the card. **However** that would re-render too late for a
   scroll-into-view — so do it more directly: add a `Store::peek` /
   `Store::get_cached` that reads via a `std::sync::mpsc::sync_channel`
   inside the existing `tokio_rusqlite` connection, OR — simpler —
   use the same `conn.call(|c| query_link_preview_blocking(c, url))`
   pattern and run it inline on the main thread. The lookup is a
   primary-key `SELECT` against SQLite; that's microseconds. Do that.
2. Build the card widget:
   * **Cached + success (status 0):** Title label (1 line, ellipsize
     end, semibold), description label (≤ 2 lines, dim), site name
     (`<sitename> · <host>`) in a caption style, thumbnail on the
     left (rounded 8 px, 80×80 cover) if `image_path` resolves. The
     whole card is a `gtk::Button` with the `flat`/`link-preview`
     CSS class — clicking it opens the URL via the same `open_uri`
     helper that `bubble_label` already uses. Cursor: `pointer`.
   * **Cached + failure (status 1):** show the URL itself in dim
     mono, no card chrome (or a "Couldn't load preview" footer).
     Actually, the bubble already shows the URL clickably. Show a
     *one-line* `caption` below it: `Couldn't load preview` (the
     `error` string truncated). Don't take up card space.
   * **Not cached (no row):** show a `gtk::Spinner` (16×16) in a
     80×80 placeholder + a grey "Loading preview…" caption, with
     the bubble's URL still the only clickable thing. As soon as
     the fetch returns and `upsert_link_preview` runs, we want the
     card to update.
3. **Scheduling the fetch.** When we hit the "not cached" branch:
   * Call `crate::preview::fetch(url.clone())` from inside
     `gtk_bridge::spawn` — the fetch is `Send` and goes to the tokio
     runtime.
   * In the `on_done` closure, call `store.upsert_link_preview(p)`.
     If `image_path` is `Some`, also `notify_preview_ready(url)` so
     the in-flight widget can re-render itself.

### 4. Live updates — turning a spinner into a card

The naive way (`reload_messages`) flickers the scroll. Instead, hold
a `HashMap<String, gtk::Widget>` of "card for this URL" on the
`Ui` struct (next to the other `borrow` cells). When the fetch
returns:

* `upsert_link_preview` writes the DB row.
* The on_done closure emits a `glib::Idle` task that walks
  `msg_container` looking for cards whose `url` matches, removes
  them, and re-runs `link_preview_card` against the just-inserted
  row. The actual `link_preview_card` is cheap and reads the same
  data we just wrote, so it can't race itself.

This is a small extra method, `Ui::refresh_link_card(url: &str)`,
called from the on_done. It walks `msg_container` once per URL, so
the cost is bounded by `O(messages_loaded)`, not `O(visible_cards)`.

### 5. Configurability

Togglable via a `Settings` flag (Schemas/TBD), but for v1 we hardcode
sensible defaults in `src/preview.rs` `const`s and read them from
`crate::config` later. Defaults:

| setting              | default | meaning                               |
|----------------------|---------|---------------------------------------|
| `max_body_bytes`     | 1 MiB   | HTML fetch cap                        |
| `max_image_bytes`    | 4 MiB   | og:image cap                          |
| `timeout`            | 10 s    | per request                           |
| `max_concurrent`     | 8       | bounded by a `tokio::Semaphore`       |
| `ttl_days`           | 7       | re-fetch rows older than this         |
| `enabled`            | true    | user-visible toggle later             |

`max_concurrent` is enforced by a `Semaphore::new(8)` in the spawn
wrapper. With the typical chat volume (a few URLs in a message
burst), we never hit it, but it bounds memory if a user opens a
chat with hundreds of historical links.

### 6. CSS additions to `CSS` in `src/ui/mod.rs`

```css
.link-preview {
  padding: 8px;
  border-radius: 12px;
  border: 1px solid alpha(currentColor, 0.08);
  background-color: alpha(currentColor, 0.03);
}
.link-preview:hover { background-color: alpha(currentColor, 0.06); }
.link-preview-thumb {
  border-radius: 8px;
  background-color: alpha(currentColor, 0.08);
  min-width: 72px; min-height: 72px;
}
.link-preview-title { font-weight: 600; }
.link-preview-desc  { color: alpha(currentColor, 0.65); }
.link-preview-host  { color: alpha(currentColor, 0.55); font-size: 0.85em; }
.link-preview-spinner { color: alpha(currentColor, 0.55); }
```

The card itself is theme-aware because everything is `currentColor`
relative.

### 7. Test plan (parallel to the existing `#[cfg(test)]` blocks)

* `store::tests::link_preview_round_trip` — insert a preview, look it
  up, confirm image_path normalization. Assert second insert with
  same `url` overwrites, second insert with different `url` doesn't.
* `preview::tests::normalize_url` — `https://Example.com/foo?utm_x=1`
  and `HTTPS://example.com/foo` collapse; `https://example.com/foo#x`
  loses the fragment.
* `preview::tests::parse_meta` — feed a fixed HTML string with mixed
  `og:*` / `twitter:*` / `<title>` / no-og and assert the right
  fields get picked, the order is right, and HTML entities are
  decoded.
* `preview::tests::rejects_private_host` — point at `127.0.0.1` and
  `192.168.0.1`, expect `status: 1` with `error: "private host"`.
  Tests can use a stub resolver via a small trait indirection.
* `preview::tests::truncates_title` — 10 000 char `og:title` is
  clipped to 200.
* `ui::tests::link_preview_card_renders_loading` (if we add a UI
  test harness) — otherwise manual: send a message containing
  `https://example.com`, confirm a spinner card appears, then a
  populated card.

### 8. File-by-file change list

| file                   | change                                                     |
|------------------------|------------------------------------------------------------|
| `Cargo.toml`           | add `ureq = { version = "2", default-features = false, features = ["tls", "gzip"] }` (already in lock as transitive — promote to direct). No new transitive deps if we hand-roll the meta parser. |
| `src/store/mod.rs`     | new `link_preview` table + migration v4; new `query_link_preview_blocking` / `upsert_link_preview_blocking`; new `Store::get_link_preview` / `upsert_link_preview`; new `Store::preview_image_dir` returning the cache dir. |
| `src/store/model.rs`   | add `pub struct LinkPreview`. |
| `src/preview.rs` (new) | fetcher: `normalize_url`, `fetch`, `parse_meta`, `decode_entities`, `in_flight: Mutex<HashMap<...>>`, `Semaphore::new(8)`. |
| `src/main.rs`          | `mod preview;`. |
| `src/ui/mod.rs`        | (a) new `link_preview_card(url, &Store)` widget builder. (b) call it from `message_body` for each URL in `body_text(m)`. (c) `Ui::refresh_link_card(url)` to live-update after a fetch lands. (d) add the new CSS classes to the `CSS` const. (e) `Ui::link_cards: RefCell<HashMap<String, gtk::Widget>>` for the live-update map. |
| `src/preview/tests.rs` (new) | the unit tests above; runs under `cargo test` without touching GTK. |

### 9. Migration safety

The store's `migrate` function runs `user_version` checks. New
migration v4 just adds the table and the index. Existing DB files
auto-upgrade on first open; no user action.

### 10. Privacy / safety review

* HTML body and image bytes are written to the per-app cache dir
  only; never re-served, never sent back to the protocol layer.
* `User-Agent` identifies the app (`openbubbles-gtk/0.1`) but
  carries no user identifiers. We do **not** add `Referer` headers.
* Private IP guard means an attacker pasting `http://10.0.0.1/…`
  in chat doesn't get used as an SSRF pivot when someone clicks
  the message.
* 1 MiB HTML / 4 MiB image caps + timeouts = a stuck `og:image`
  download can never block a render or fill the disk.
* A failed preview is cached as `status: 1`; we don't re-hit the
  network on every scroll. The TTL refreshes it after 7 days so
  transient outages self-heal.
* We don't fetch from `localhost` even via redirect — the resolver
  guard rejects loopback, link-local, and the CGNAT range
  (`100.64.0.0/10`).

### 11. Rollout

1. Land the `link_preview` table and `Store` accessors first — no
   behaviour change. (`cargo test` covers the round-trip.)
2. Land the fetcher behind a `preview::fetch` that's unused.
   (`cargo test` covers parsing, normalization, private-host guard.)
3. Wire `message_body` → `link_preview_card` reading from the cache
   only. Nothing fetches yet, so behaviour is "show the URL
   inline as before". (Manual smoke test: existing chats look the
   same.)
4. Wire the scheduler: cards with no cached row show the spinner
   and spawn the fetch. (Manual: open a chat with a
   never-seen-before URL; spinner → card in ~1 s. Old chats with
   previously-rendered URLs render immediately because their rows
   are already cached.)
5. Wire live updates (`Ui::refresh_link_card`) so the spinner
   becomes the card in place. (Manual: a chat with 5 fresh URLs
   in one message — all 5 cards materialise in place, no scroll
   jump.)
6. Add the CSS, polish the layout, ship.

### 12. Out of scope (deliberately)

* Per-domain rate limits / robots.txt (low signal, easy to add
  later if abused).
* YouTube/Twitter-specific embeds (the OG meta we get back is good
  enough — no special-casing in v1).
* Server-side previews (we don't have a server; we fetch from the
  sender's link like every other client).
* Card re-render on `text-scale` change. The current `apply_text_scale`
  is per-widget; we'd re-run `link_preview_card` once per scale
  change. Easy follow-up.
* Caching previews across DB resets. Cache dir lives under
  `XDG_CACHE_HOME` and survives an app uninstall — useful, not
  dangerous.
