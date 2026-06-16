# Implementation spec: inline link previews (rich URL cards) in openbubbles-gtk

> **For the implementing agent:** this is a complete, self-contained spec. Read all of it
> before writing code. The architecture here is **deliberately different** from a naive
> "fetch the URL and parse Open Graph tags" approach — for an iMessage client most of the
> work is already done for you by rustpush, and fetching incoming links is a privacy bug,
> not a feature. Sections marked **MUST** and **MUST NOT** are hard constraints; do not
> "simplify" them away. Where a signature is uncertain, the spec says *verify against the
> actual code* — do that, don't guess.

---

## 0. TL;DR

There are two completely separate paths, and they share almost nothing:

1. **Incoming links (do this first, it's the big win).** rustpush already fetches, decodes,
   and hands you the sender-generated preview (title, summary, URL, and the thumbnail bytes)
   on every received message via `NormalMessage.link_meta`. **You do not fetch anything.**
   You persist what rustpush gave you and render a card. No network, no SSRF surface, no
   privacy leak, no main-thread blocking.

2. **Outgoing links (do this second).** When the *local user* composes a message containing
   a URL, generate the preview at send time: fetch the page once, populate an
   `LPLinkMetadata`, attach it via `NormalMessage.link_meta`, and let rustpush ship it so
   real iMessage peers also see a card. A real HTTP fetch belongs **only** here (plus an
   optional fallback in §6).

The standalone HTTP fetcher + Open Graph parser is scoped to path 2 only. It is **not** the
core mechanism.

---

## 1. Background: how iMessage link previews actually work

iMessage rich links are generated **on the sender's device** (Apple's LinkPresentation
framework produces an `LPLinkMetadata`: title, summary, image), and that metadata + the
thumbnail are **transmitted as part of the message**. The recipient renders the embedded
preview and **never fetches the URL itself**. Signal and WhatsApp do the same. Only naive
clients fetch incoming links — doing so leaks the recipient's IP and read-timing to a
sender-controlled server (a tracking-beacon / deanonymization primitive). We will not do that.

rustpush implements the full Apple wire format for this — both decode (incoming) and encode
(outgoing). Everything in §3 is already in the dependency; we are consuming it, not building it.

---

## 2. Non-negotiable constraints (read before coding)

- **MUST NOT** auto-fetch URLs that arrive in *incoming* messages. Incoming previews come
  from `link_meta` only. (Privacy. See §1.)
- **MUST NOT** perform synchronous/blocking store reads on the GTK main thread during
  render. The store is `tokio_rusqlite` (a channel hop to a worker). Per-card blocking
  lookups during `populate_messages` cause jank and can deadlock the glib loop. Load preview
  data **with the message**, in the existing async load, into an in-memory structure the
  renderer reads synchronously. (See §5.3.)
- **MUST NOT** call a blocking HTTP client (e.g. `ureq`) from inside a plain
  `tokio::task::spawn` — that blocks a runtime worker. Use an async client (`reqwest`, which
  is already a transitive dep) or `tokio::task::spawn_blocking`. Prefer `reqwest`. (See §6.)
- **MUST** validate the SSRF guard at *connection* time and on *every redirect hop*, not once
  up front. Resolve → reject private/loopback/link-local/CGNAT → connect to the validated IP.
  A "resolve once then let the client reconnect" guard is TOCTOU and is bypassed by redirects.
  (See §6.)
- **MUST** treat an incoming embedded preview as the **sender's static snapshot**, keyed by
  message, with no TTL and no re-fetch. Only *fetched* previews (path 2) get a URL-keyed TTL
  cache. (See §4.)

---

## 3. What rustpush already gives you (the API surface)

All types are re-exported from the crate root (`rustpush::...`); see `third_party/rustpush/src/lib.rs:47`.

### 3.1 The message shape

```rust
// third_party/rustpush/src/imessage/messages.rs
pub enum Message {
    Message(NormalMessage),         // normal text/attachment/preview message
    UpdateExtension(UpdateExtensionMessage), // balloon fill-in / update (see §5.4)
    // ... reactions, edits, typing, etc.
}

pub struct NormalMessage {
    pub parts: MessageParts,
    pub service: MessageType,
    pub app: Option<ExtensionApp>,
    pub link_meta: Option<LinkMeta>,   // <-- THE URL PREVIEW LIVES HERE
    // ... effect, reply_guid, subject, voice, scheduled, embedded_profile
}

pub struct LinkMeta {
    pub data: LPLinkMetadata,
    pub attachments: Vec<Vec<u8>>,     // raw image blobs, INLINE (already downloaded)
}
```

### 3.2 The metadata

```rust
// third_party/rustpush/src/imessage/rawmessages.rs
pub struct LPLinkMetadata {
    pub title: Option<String>,
    pub summary: Option<String>,                 // the "description"
    pub url: Option<NSURL>,                       // canonical (post-redirect) URL
    pub original_url: Option<NSURL>,             // what the sender typed
    pub image: Option<RichLinkImageAttachmentSubstitute>,  // index into LinkMeta.attachments
    pub icon:  Option<RichLinkImageAttachmentSubstitute>,
    pub image_metadata: Option<LPImageMetadata>, // dimensions/type, NOT the bytes
    pub icon_metadata:  Option<LPIconMetadata>,
    pub is_incomplete: Option<bool>,             // placeholder marker (see §5.4)
    pub specialization2: Option<LPSpecializationMetadata>, // tweet/Mastodon/Threads special-casing
    // ... images, icons, version, flags
}

pub struct RichLinkImageAttachmentSubstitute {
    pub rich_link_image_attachment_substitute_index: u64, // index into LinkMeta.attachments
}
```

### 3.3 The critical gotcha: the thumbnail is inline bytes, indexed

The preview image is **not** a normal MMCS attachment on the message. It is a raw blob inside
`LinkMeta.attachments`, and `LPLinkMetadata.image` is a *substitute* that holds the index into
that vec. rustpush already pulled the balloon body (via MMCS if it was large — that async step
happens inside `MessageInst::from_raw`) so by the time you see the message the bytes are in hand.

```rust
fn preview_image_bytes(lm: &LinkMeta) -> Option<&[u8]> {
    let idx = lm.data.image.as_ref()?.rich_link_image_attachment_substitute_index as usize;
    lm.attachments.get(idx).map(|v| v.as_slice())   // PNG/JPEG bytes
}
```

`NSURL` is a wrapper — **verify** how it exposes the string (likely a public field or a
`Display`/`as_str`); extract the `String` for storage and for the open-on-click action.

### 3.4 Decode is automatic

Every received message passes through `MessageInst::from_raw`
(`third_party/rustpush/src/imessage/aps_client.rs:300`). The URL-balloon branch
(`messages.rs:2851`, bundle id `com.apple.messages.URLBalloonProvider`) ungzips the balloon,
NSKeyed-unarchives it into a `RichLink`, and populates `link_meta`. **You do not call any of
this.** You just read `nm.link_meta` on the message you already receive. The decode path emits
`debug!("a".."e")` breadcrumbs and `error!("Error parsing url preview! {e}")` on failure — if a
real link renders blank, those tell you which step failed.

---

## 4. Data model

Two distinct stores. Do not conflate them.

### 4.1 Embedded previews (incoming) — message-scoped, no TTL

An embedded preview is the sender's snapshot, tied to a specific message. Store it next to the
message, keyed by message id, **not** by URL. No expiry, no refetch.

Simplest option — fold into the message store as a sidecar table (DDL in `src/store/mod.rs`,
migration via `user_version`, matching the existing pattern):

```sql
CREATE TABLE message_link_preview(
  message_guid TEXT NOT NULL,     -- FK to messages
  part_idx     INTEGER NOT NULL,  -- which part/URL, if a message can have >1
  url          TEXT,              -- LPLinkMetadata.url (or original_url)
  title        TEXT,
  summary      TEXT,
  image_path   TEXT,              -- cached thumbnail file, or NULL
  is_placeholder INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (message_guid, part_idx)
);
```

Write the thumbnail bytes from `LinkMeta.attachments[idx]` into
`$XDG_CACHE_HOME/openbubbles-gtk/previews/<message_guid>-<part_idx>.<ext>` and store the path.
(You may instead store the bytes inline as a BLOB — fine for small thumbnails — but a file keeps
the row small and matches how attachments are likely already handled. Match the existing pattern.)

### 4.2 Fetched previews (outgoing + fallback) — URL-keyed, TTL

This is the only place the original URL-keyed cache from the earlier plan survives:

```sql
CREATE TABLE link_preview(
  url          TEXT PRIMARY KEY,  -- normalized
  title        TEXT,
  description  TEXT,
  site_name    TEXT,
  image_path   TEXT,
  fetched_at   INTEGER NOT NULL,  -- unix ms; TTL
  status       INTEGER NOT NULL,  -- 0 ok, 1 failed
  error        TEXT
);
```

Bump `user_version` once (e.g. v4) and create both tables in the migration.

---

## 5. Part A — Incoming previews (primary path)

### 5.1 Ingest: rustpush message → store

At the boundary where a rustpush `MessageInst` is converted into your `StoredMessage`
(in `src/protocol/` — find the real rustpush-backed handler, not `stub.rs`), extract `link_meta`:

```rust
if let Message::Message(nm) = &inst.message {
    if let Some(lm) = &nm.link_meta {
        let url     = lm.data.url.as_ref().or(lm.data.original_url.as_ref())
                         .map(/* NSURL -> String, verify accessor */);
        let title   = lm.data.title.clone();
        let summary = lm.data.summary.clone();
        let is_placeholder = lm.data.is_incomplete.unwrap_or(false);

        // thumbnail: inline bytes, indexed (see §3.3)
        let image_path = preview_image_bytes(lm).map(|bytes| {
            write_preview_image(&message_guid, part_idx, bytes) // -> cache path; sniff ext
        });

        store.upsert_message_link_preview(MessageLinkPreview {
            message_guid, part_idx, url, title, summary, image_path, is_placeholder,
        }).await?;
    }
}
```

Do this during the **async** receive/ingest, alongside however attachments and message rows are
already persisted. Nothing here touches the GTK thread.

### 5.2 Load: batch, with the messages

When the UI loads a window of messages (the existing async `messages_from` / whatever feeds
`populate_messages`), load their previews **in the same async pass** and hand the renderer an
in-memory map:

```rust
// async, off the GTK thread:
let msgs = store.messages_from(...).await?;
let guids: Vec<_> = msgs.iter().map(|m| m.guid.clone()).collect();
let previews: HashMap<(String, i64), MessageLinkPreview> =
    store.message_link_previews_for(&guids).await?;   // one query, WHERE message_guid IN (...)
// pass `previews` into populate_messages
```

The renderer then reads from `previews` synchronously — **no store call on the main thread.**

### 5.3 Render: the card widget

Extend `message_body` in `src/ui/mod.rs`. After the existing text bubble, for each preview the
message has, append a `link_preview_card(...)` built from the **already-loaded** `MessageLinkPreview`:

```text
message_body(m, own, &previews)
├── (existing) attachments
├── (existing) bubble -> bubble_label(body_text(m))   // clickable links already work
└── for each preview of m:
    └── link_preview_card(preview)                      // new; reads from in-memory data only
```

`link_preview_card`:
- A `gtk::Button` with a flat `.link-preview` CSS class; clicking opens `url` via the same
  `open_uri` helper `bubble_label` already uses. Cursor `pointer`.
- Layout: thumbnail on the left (rounded ~8px, ~72–80px, cover), then title (semibold, 1 line,
  ellipsize end), summary (dim, ≤2 lines), and a host caption derived from `url`.
- Thumbnail: decode the cached image to a texture. Prefer bytes/texture over `Texture::from_file`
  to avoid a synchronous file decode on the main thread; if you store a path, load it on a worker
  and set it when ready. **Verify** the exact gtk4-rs call:
  ```rust
  // from inline bytes (preferred if you kept them):
  let texture = gdk::Texture::from_bytes(&glib::Bytes::from(bytes)).ok();
  // or from a path via a background decode + gdk_pixbuf::Pixbuf, then Texture::for_pixbuf
  ```
- If `title`/`summary` are both absent (placeholder, §5.4) show a compact "loading preview…"
  state instead of an empty card.

CSS to add to the `CSS` const in `src/ui/mod.rs` (theme-aware via `currentColor`):

```css
.link-preview { padding: 8px; border-radius: 12px;
  border: 1px solid alpha(currentColor, 0.08);
  background-color: alpha(currentColor, 0.03); }
.link-preview:hover { background-color: alpha(currentColor, 0.06); }
.link-preview-thumb { border-radius: 8px; min-width: 72px; min-height: 72px;
  background-color: alpha(currentColor, 0.08); }
.link-preview-title { font-weight: 600; }
.link-preview-desc  { color: alpha(currentColor, 0.65); }
.link-preview-host  { color: alpha(currentColor, 0.55); font-size: 0.85em; }
```

### 5.4 Lifecycle: placeholder → fill-in

iMessage often sends the balloon in **two stages**: a placeholder first
(`LPLinkMetadata.is_incomplete == true` / `rich_link_is_placeholder`), then a fill-in once the
sender's device finishes generating the real preview. The fill-in arrives as a separate message —
likely `Message::UpdateExtension(UpdateExtensionMessage)` (`messages.rs:1614`) or a follow-up with
the same guid. **Verify the exact mechanism** by logging received messages while you send yourself
a test link.

Handle it:
- Persist the placeholder (store `is_placeholder = 1`); render the compact loading state.
- On the update, upsert the same `(message_guid, part_idx)` row with the real fields and refresh
  the card **in place** (walk `msg_container`, find the card for that message/part, rebuild it from
  the new stored row) — same in-place refresh the earlier plan wanted, now driven by a protocol
  event instead of a fetch completing. Do **not** `reload_messages` (it flickers/jumps scroll).

---

## 6. Part B/C — Outgoing previews and the fetcher

### 6.1 When to fetch

Fetch **only** for (a) a URL the local user is sending, at send time, and (b) optionally, as an
**opt-in** fallback for incoming messages that have *no* `link_meta` (e.g. SMS/RCS/stripped). The
fallback must be off by default and clearly a user setting, because it reintroduces the incoming
fetch/privacy exposure §1 warns about.

### 6.2 Fetcher module `src/preview.rs`

- HTTP: **`reqwest`** (async; already in `Cargo.lock` transitively — promote to a direct dep).
  Do **not** use `ureq` in a `tokio::spawn`.
- Parse: extract `og:title` / `og:description` (fallback `twitter:*`, `<title>`, meta description),
  `og:site_name` (fallback URL host), `og:image`. Use a real parser, not hand-rolled regex —
  e.g. the `webpage` or `link-preview` crate (run their fetch on the tokio runtime or via
  `spawn_blocking`), or `reqwest` + a lightweight HTML parser (`tl`) and pull the meta tags
  yourself. Decode HTML entities properly (the parser handles this; a 5-entity hand-rolled
  decoder is insufficient).
- Caps: 1 MiB HTML body, 4 MiB image, 10s timeouts, follow ≤3 redirects **manually**.
- **SSRF (MUST, see §2):** disable automatic redirects (`redirect::Policy::none()`); for each hop
  resolve the host (`tokio::net::lookup_host`), reject if **any** resolved address is private,
  loopback, link-local, or CGNAT (`100.64.0.0/10`), then pin the connection to a validated address
  (`reqwest::ClientBuilder::resolve(host, validated_addr)`), set the `Host` header, and only then
  GET. Re-run this on every redirect target. Also reject non-`http(s)` schemes.
- Headers: a `User-Agent` that names the app only (`openbubbles-gtk/<version>`); **no** `Referer`,
  no user identifiers.
- Dedup: an in-flight `Lazy<Mutex<HashMap<String, ...>>>` keyed on the normalized URL; a
  `tokio::Semaphore` to bound concurrency (~8).
- Cache: write results to the §4.2 `link_preview` table; cache failures as `status = 1` so a
  404 isn't re-hit every render; TTL-refresh after ~7 days.
- Normalize URLs for the cache key: lowercase scheme+host, drop fragment, drop default ports,
  strip `utm_*`.

### 6.3 Sending the generated preview

Build the metadata yourself and let rustpush own the wire format (it archives a `RichLink` into the
`URLBalloonProvider` balloon — `messages.rs:2236-2241`):

```rust
let mut md = LPLinkMetadata::default(); // verify constructibility / required fields
md.title = Some(fetched.title);
md.summary = Some(fetched.description);
md.url = Some(/* NSURL from the user's URL */);
md.image = Some(RichLinkImageAttachmentSubstitute { rich_link_image_attachment_substitute_index: 0 });
nm.link_meta = Some(LinkMeta { data: md, attachments: vec![image_bytes] });
// send nm via the normal send path
```

The `image` substitute index must point into the `attachments` vec you supply (symmetric to the
decode in §3.3). If constructing `LPLinkMetadata` directly is awkward (many fields), **verify**
whether rustpush exposes a higher-level helper; otherwise fill the fields you have and leave the
rest `None`/default.

---

## 7. Tests

- `store`: round-trip `message_link_preview` (insert, batch-load by guids, placeholder→fill-in
  overwrite). Round-trip `link_preview` (URL-keyed, failure cached).
- `preview` (pure, no network): URL normalization; OG/meta extraction from a fixed HTML fixture
  (og / twitter / `<title>` / missing); **SSRF guard rejects** `127.0.0.1`, `10.0.0.1`,
  `192.168.1.1`, `169.254.x`, `100.64.x` — inject a stub resolver so it's deterministic and offline;
  title/summary truncation.
- Incoming render: a synthetic `MessageLinkPreview` produces a card; a placeholder produces the
  loading state; the fill-in replaces it in place.
- Manual smoke: send yourself (from a real Messages client) a `https://github.com` link; confirm a
  card with title + thumbnail appears with **no outbound HTTP** from the app (watch the network /
  the absence of a `preview::fetch` call). Then compose a link locally and confirm a peer on real
  iMessage sees a card.

---

## 8. Phased rollout (each phase independently shippable)

1. **Schema + ingest, no UI.** Add both tables + migration; persist `link_meta` on receive. No
   behaviour change. (`cargo test` covers store round-trips.)
2. **Render incoming cards** from stored data via the batch-loaded map. Biggest visible win, zero
   network. (Manual: existing chats with links now show cards; old chats too, once re-ingested.)
3. **Placeholder/update refresh** in place (§5.4).
4. **Outgoing generation** at send (§6.3) — links you send now carry previews to peers.
5. **Fetcher + opt-in incoming fallback** (§6.2), behind a setting, default off.

Stop after phase 3 and you already have a feature that matches macOS Messages for received links.

---

## 9. File-by-file change list

| file | change |
|------|--------|
| `Cargo.toml` | promote `reqwest` to a direct dep (async, already transitive); add an OG/HTML parser (`webpage` / `link-preview` / `tl`) only when implementing §6. |
| `src/store/mod.rs` | `message_link_preview` + `link_preview` tables; `user_version` migration; async accessors: `upsert_message_link_preview`, `message_link_previews_for(&[guid])` (batched), `get_link_preview`, `upsert_link_preview`; a `preview_image_dir()` helper. |
| `src/store/model.rs` | `MessageLinkPreview` and `LinkPreview` structs. |
| `src/protocol/<real backend>.rs` | extract `nm.link_meta` on receive, write thumbnail bytes to cache, upsert the message-scoped preview; handle the §5.4 update message. |
| `src/ui/mod.rs` | `link_preview_card(&MessageLinkPreview)`; call it from `message_body` per preview using the batch-loaded map; in-place `refresh_link_card(message_guid, part_idx)`; new CSS in the `CSS` const; thread the `previews` map through `populate_messages`. |
| `src/preview.rs` (new, phase 5/4) | async fetcher: normalize, fetch (`reqwest`, manual redirects), SSRF guard (resolve+validate+pin per hop), OG parse, image download, in-flight dedup, semaphore, TTL cache writes. |
| `src/main.rs` | `mod preview;` (when added). |

---

## 10. Out of scope (v1)

- robots.txt / per-domain rate limiting (add later if abused).
- Special embeds for YouTube/Twitter/etc. beyond what `LPLinkMetadata.specialization2` already
  carries.
- Re-rendering cards on text-scale change (follow-up; current scaling is per-widget).
- Server-side unfurling (there is no server in this path).

---

## 11. Acceptance criteria

- A received iMessage containing a URL renders a card with the sender's title/summary/thumbnail,
  and the app makes **zero** outbound HTTP requests to display it.
- Scrolling a long history of link messages does not jank; there are **no** synchronous store
  reads on the GTK main thread (previews are batch-loaded with the messages).
- A placeholder preview upgrades to the full card in place without a scroll jump.
- A URL composed and sent locally appears as a rich card to a recipient on real iMessage.
- The SSRF unit tests pass offline, rejecting all private/loopback/link-local/CGNAT targets,
  including via a redirect hop.
- No code path fetches a URL that originated in an incoming message unless the user has explicitly
  enabled the fallback setting.
