//! Conversations UI, styled after Fractal: an `AdwNavigationSplitView` with an
//! avatar-led sidebar (unread badges) on the left and, on the right, a flat
//! sender-grouped message timeline plus a compose bar.
//!
//! Presentation only — the store, receive loop, and send paths are untouched.
//! Everything here reads from [`crate::store::Store`] and refreshes when the
//! backend pulses the notifier.

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::rc::Rc;
use std::sync::{Arc, Once};

use adw::prelude::*;

use crate::gtk_bridge;
use crate::protocol::{Backend, Connection, ImClient};
use crate::store::{
    AttachmentRecord, ChatRef, ChatSummary, IncomingMessage, Ingest, Store, StoredAttachment,
    StoredMessage,
};

/// Phase D default: send read receipts when a chat is viewed. Becomes a user
/// setting once a settings module exists.
const SEND_READ_RECEIPTS: bool = true;

/// New sender header after this idle gap (5 min), even for the same person.
const GROUP_GAP_MS: i64 = 5 * 60 * 1000;
/// How many messages to load per page (initial open and each scroll-up).
const PAGE_SIZE: i64 = 20;
/// How long the "New Messages" divider lingers after a chat is opened and read
/// before it dismisses itself.
const UNREAD_DIVIDER_TTL_SECS: u64 = 4;

const CSS: &str = "
.unread-badge {
  background-color: @accent_bg_color;
  color: @accent_fg_color;
  border-radius: 999px;
  padding: 0px 7px;
  margin: 4px 2px;
  font-weight: bold;
  font-size: 0.85em;
}
.sender-name {
  font-weight: bold;
  font-size: 0.9em;
  color: @accent_color;
  margin-left: 2px;
}
.unread-marker {
  color: @accent_color;
  font-size: 0.8em;
  font-weight: bold;
}
.bubble {
  border-radius: 18px;
  padding: 6px 12px;
}
.bubble-in {
  background-color: #e7e7ea;
  color: #161616;
}
.bubble-out {
  background-color: #1b7ffb;
  color: #ffffff;
}
.attachment-image {
  border-radius: 14px;
  background-color: #00000010;
}
.lightbox-dim {
  background-color: rgba(0, 0, 0, 0.8);
}
.unread-pill {
  padding: 4px 14px;
  font-size: 0.9em;
}
";

/// Cheap-to-clone bundle the UI closures share.
#[derive(Clone)]
struct Ui {
    store: Store,
    backend: Arc<dyn Backend>,
    split: adw::NavigationSplitView,
    content_page: adw::NavigationPage,
    chat_list: gtk::ListBox,
    chats: Rc<RefCell<Vec<ChatSummary>>>,
    msg_container: gtk::Box,
    scroller: gtk::ScrolledWindow,
    client: ImClient,
    connection: Connection,
    handles: Vec<String>,
    open_summary: Rc<RefCell<Option<ChatSummary>>>,
    // Pagination state for the open chat.
    page_oldest: Rc<RefCell<Option<(i64, i64)>>>,
    page_has_more: Rc<RefCell<bool>>,
    page_loading: Rc<RefCell<bool>>,
    // First-unread anchor for the open chat: (guid, date). The divider is placed
    // before this message; while it isn't loaded, the floating pill is shown.
    unread: Rc<RefCell<Option<(String, i64)>>>,
    unread_marker_shown: Rc<RefCell<bool>>,
    // Handle to the drawn divider widget (so it can be removed in place) and a
    // generation guard for its self-dismiss timer.
    unread_marker: Rc<RefCell<Option<gtk::Widget>>>,
    unread_dismiss_gen: Rc<Cell<u64>>,
    unread_pill: gtk::Button,
    // Whether the window currently has focus. Messages that arrive while it
    // doesn't are held as unread until the user comes back.
    focused: Rc<Cell<bool>>,
    // While a rebuild's layout settles, transient scroll resets must not toggle
    // the bottom-follow. `settling` suppresses that; `settle_gen` lets the latest
    // scroll request own the clear so overlapping rebuilds don't end it early.
    settling: Rc<Cell<bool>>,
    settle_gen: Rc<Cell<u64>>,
    // Coalesces the receive-loop's per-message refresh pulses so a burst (e.g.
    // the backlog drained on startup) collapses into a single sidebar rebuild
    // instead of flickering through dozens.
    refresh_gen: Rc<Cell<u64>>,
    // Desktop notifications: highest message date already considered, and the
    // toplevel window (for raising it when a notification is clicked).
    notify_watermark: Rc<Cell<i64>>,
    window: Rc<RefCell<Option<gtk::Window>>>,
    // Chats we currently have a desktop notification posted for, so we can
    // withdraw it once the chat is read — including reads synced from another
    // device, which clear unread without us opening the chat here.
    notified_chats: Rc<RefCell<HashSet<i64>>>,
    // Cleared once on the first chat load, to sweep stale notifications left in
    // the center by a previous session (read elsewhere while we were closed).
    notify_swept: Rc<Cell<bool>>,
}

/// Swap the window over to the messaging UI and start receiving. Called once a
/// live session exists (restored or freshly registered).
pub fn enter_messaging(
    nav: &adw::NavigationView,
    backend: &Arc<dyn Backend>,
    store: Store,
    connection: Connection,
    client: ImClient,
    handles: Vec<String>,
) {
    install_css();

    // --- sidebar (chat list) ---
    let chat_list = gtk::ListBox::new();
    chat_list.add_css_class("navigation-sidebar");
    chat_list.set_selection_mode(gtk::SelectionMode::Single);
    chat_list.set_activate_on_single_click(true);
    let sidebar = page("Messages", &scrolled(&chat_list), None);

    // --- content (persistent timeline + compose) ---
    let msg_container = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(0)
        .margin_top(8)
        .margin_bottom(8)
        .build();
    let msg_scroller = scrolled(&msg_container);

    // Floating "more unread above" pill, layered over the timeline. Hidden until
    // a chat with not-yet-loaded unread messages is opened.
    let unread_pill = gtk::Button::builder()
        .label("↑ Earlier unread messages")
        .halign(gtk::Align::Center)
        .valign(gtk::Align::Start)
        .margin_top(10)
        .visible(false)
        .build();
    unread_pill.add_css_class("osd");
    unread_pill.add_css_class("pill");
    unread_pill.add_css_class("unread-pill");

    let msg_overlay = gtk::Overlay::new();
    msg_overlay.set_child(Some(&msg_scroller));
    msg_overlay.add_overlay(&unread_pill);

    let attach = gtk::Button::from_icon_name("text-x-generic-symbolic");
    attach.add_css_class("flat");
    attach.set_tooltip_text(Some("Attach a file"));
    let entry = gtk::Entry::builder()
        .hexpand(true)
        .placeholder_text("Message")
        .build();
    // GTK's built-in emoji picker: a dim emoji glyph inside the entry (right
    // side) that opens the chooser and inserts into the text — functional.
    entry.set_show_emoji_icon(true);
    let send = gtk::Button::from_icon_name("ob-send-symbolic");
    send.add_css_class("circular");
    send.add_css_class("suggested-action");
    send.set_tooltip_text(Some("Send"));

    let compose = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(6)
        .margin_top(6)
        .margin_bottom(6)
        .margin_start(8)
        .margin_end(8)
        .build();
    compose.append(&attach);
    compose.append(&entry);
    compose.append(&send);

    let content_page = page("Select a chat", &msg_overlay, Some(compose.upcast_ref()));

    // --- split view ---
    let split = adw::NavigationSplitView::new();
    split.set_sidebar(Some(&sidebar));
    split.set_content(Some(&content_page));

    let ui = Ui {
        store: store.clone(),
        backend: backend.clone(),
        split: split.clone(),
        content_page: content_page.clone(),
        chat_list: chat_list.clone(),
        chats: Rc::new(RefCell::new(Vec::new())),
        msg_container: msg_container.clone(),
        scroller: msg_scroller.clone(),
        client: client.clone(),
        connection: connection.clone(),
        handles: handles.clone(),
        open_summary: Rc::new(RefCell::new(None)),
        page_oldest: Rc::new(RefCell::new(None)),
        page_has_more: Rc::new(RefCell::new(false)),
        page_loading: Rc::new(RefCell::new(false)),
        unread: Rc::new(RefCell::new(None)),
        unread_marker_shown: Rc::new(RefCell::new(false)),
        unread_marker: Rc::new(RefCell::new(None)),
        unread_dismiss_gen: Rc::new(Cell::new(0)),
        unread_pill: unread_pill.clone(),
        focused: Rc::new(Cell::new(true)),
        settling: Rc::new(Cell::new(false)),
        settle_gen: Rc::new(Cell::new(0)),
        refresh_gen: Rc::new(Cell::new(0)),
        // Start the watermark at "now" so the startup backlog (past-dated) doesn't
        // fire a flood of notifications; only messages arriving live will notify.
        notify_watermark: Rc::new(Cell::new(now_ms())),
        window: Rc::new(RefCell::new(None)),
        notified_chats: Rc::new(RefCell::new(HashSet::new())),
        notify_swept: Rc::new(Cell::new(false)),
    };

    // Open a chat when its row is activated.
    {
        let ui = ui.clone();
        chat_list.connect_row_activated(move |_list, row| {
            let idx = row.index();
            if idx < 0 {
                return;
            }
            let chat = ui.chats.borrow().get(idx as usize).cloned();
            if let Some(chat) = chat {
                ui.open_chat(&chat);
            }
        });
    }

    // Load the previous page when the user scrolls near the top (ignoring the
    // transient resets a rebuild produces while it settles).
    {
        let ui = ui.clone();
        let adj = msg_scroller.vadjustment();
        adj.connect_value_changed(move |a| {
            if ui.settling.get() {
                return;
            }
            // Only a genuine near-top with real scrollback counts — a transient
            // reset during a rebuild collapses upper to the viewport and is ignored.
            if a.value() <= 64.0 && a.upper() > a.page_size() + 4.0 {
                ui.maybe_load_older();
            }
        });
    }

    // Tapping the floating pill jumps straight to the first unread message.
    {
        let ui = ui.clone();
        unread_pill.connect_clicked(move |_| ui.jump_to_first_unread());
    }

    // Compose send (button + Enter).
    {
        let ui = ui.clone();
        let entry = entry.clone();
        send.connect_clicked(move |_| ui.compose_send(&entry));
    }
    {
        let ui = ui.clone();
        let entry2 = entry.clone();
        entry.connect_activate(move |_| ui.compose_send(&entry2));
    }

    // Attach: open the system file picker, then upload + send the chosen file.
    {
        let ui = ui.clone();
        attach.connect_clicked(move |btn| {
            let dialog = gtk::FileDialog::builder().title("Attach a file").build();
            let win = btn
                .root()
                .and_then(|r| r.downcast::<gtk::Window>().ok());
            let ui = ui.clone();
            dialog.open(win.as_ref(), gtk::gio::Cancellable::NONE, move |res| {
                if let Ok(file) = res {
                    if let Some(path) = file.path() {
                        ui.send_file(path);
                    }
                }
            });
        });
    }

    // Track window focus once the UI is mapped into a window. Messages arriving
    // while unfocused stay unread; on return we re-show the chat with the unread
    // divider (and pill, if they're past the loaded window).
    {
        let ui_focus = ui.clone();
        let connected = Rc::new(Cell::new(false));
        split.connect_map(move |w| {
            if connected.replace(true) {
                return;
            }
            if let Some(win) = w.root().and_then(|r| r.downcast::<gtk::Window>().ok()) {
                *ui_focus.window.borrow_mut() = Some(win.clone());
                ui_focus.focused.set(win.is_active());
                let ui2 = ui_focus.clone();
                win.connect_is_active_notify(move |win| {
                    let active = win.is_active();
                    let was = ui2.focused.replace(active);
                    if active && !was {
                        ui2.on_window_focus();
                    }
                });
            }
        });
    }

    // Clicking a desktop notification raises the window and opens the chat it
    // targets (the notification carries the chat id as its action target).
    if let Some(app) = gtk::gio::Application::default() {
        let action = gtk::gio::SimpleAction::new("open-chat", Some(glib::VariantTy::INT64));
        let ui_act = ui.clone();
        action.connect_activate(move |_, param| {
            if let Some(id) = param.and_then(|p| p.get::<i64>()) {
                ui_act.activate_chat(id);
            }
        });
        app.add_action(&action);
    }

    // Host the split view inside the existing navigation stack, wrapped in an
    // overlay so we can layer an enlarged-image lightbox over everything.
    let overlay = gtk::Overlay::new();
    overlay.set_widget_name("lightbox-host");
    overlay.set_child(Some(&split));
    let host = adw::NavigationPage::builder()
        .title("OpenBubbles")
        .child(&overlay)
        .build();
    nav.replace(&[host]);

    ui.reload_chats();

    // Receive loop -> persist -> pulse -> refresh.
    let (tx, rx) = async_channel::unbounded::<()>();
    backend.start_receiving(&connection, &client, handles, store, tx);
    let ui_refresh = ui.clone();
    gtk_bridge::forward(rx, move |()| ui_refresh.schedule_refresh());
}

impl Ui {
    fn reload_chats(&self) {
        let store = self.store.clone();
        let ui = self.clone();
        gtk_bridge::spawn(async move { store.chats().await }, move |res| {
            let chats = res.unwrap_or_else(|e| {
                eprintln!("chats load error: {e:#}");
                Vec::new()
            });
            clear(&ui.chat_list);
            for c in &chats {
                ui.chat_list.append(&chat_row(c, &ui.handles));
            }
            // Keep the open chat highlighted across refreshes.
            if let Some(open) = ui.open_summary.borrow().as_ref() {
                if let Some(i) = chats.iter().position(|c| c.id == open.id) {
                    if let Some(row) = ui.chat_list.row_at_index(i as i32) {
                        ui.chat_list.select_row(Some(&row));
                    }
                }
            }
            // Withdraw notifications for chats that are no longer unread — covers
            // reads synced from another device (receipt or self-sent reply), which
            // clear unread here without us opening the chat.
            let read_off: Vec<i64> = if !ui.notify_swept.replace(true) {
                // First load: also clear any stale notification for an already-read
                // chat, in case it lingered from a previous session.
                chats.iter().filter(|c| c.unread == 0).map(|c| c.id).collect()
            } else {
                let notified = ui.notified_chats.borrow();
                notified
                    .iter()
                    .copied()
                    .filter(|&id| !chats.iter().any(|c| c.id == id && c.unread > 0))
                    .collect()
            };
            for id in read_off {
                ui.withdraw_chat_notification(id);
            }
            *ui.chats.borrow_mut() = chats;
        });
    }

    fn open_chat(&self, chat: &ChatSummary) {
        *self.open_summary.borrow_mut() = Some(chat.clone());
        self.content_page.set_title(&chat_title(chat, &self.handles));
        self.split.set_show_content(true);
        // Opening the chat means reading it — clear any pending notification.
        self.withdraw_chat_notification(chat.id);

        let store = self.store.clone();
        let backend = self.backend.clone();
        let client = self.client.clone();
        let ui = self.clone();
        let chat_id = chat.id;
        let is_group = chat.is_group;
        let chat_ref = chat_ref_of(chat);
        let my_handle = self_handle(&chat.participants, &self.handles);

        gtk_bridge::spawn(
            async move {
                // Capture the unread boundary BEFORE acking, then load and ack.
                let first = store.first_unread_incoming(chat_id).await.ok().flatten();
                let latest = store.latest_unread_incoming(chat_id).await.ok().flatten();
                let msgs = store
                    .messages_page(chat_id, None, PAGE_SIZE)
                    .await
                    .unwrap_or_default();
                if let Some((_, date)) = &latest {
                    let _ = store.mark_read_through(chat_id, *date).await;
                }
                (msgs, first, latest.map(|(g, _)| g))
            },
            move |(msgs, first, receipt_guid)| {
                // Reset pagination for the newly opened chat.
                *ui.page_oldest.borrow_mut() = msgs.first().map(|m| (m.date, m.id));
                *ui.page_has_more.borrow_mut() = msgs.len() as i64 >= PAGE_SIZE;
                *ui.page_loading.borrow_mut() = false;
                *ui.unread.borrow_mut() = first.clone();

                let anchor = first.as_ref().map(|(g, _)| g.as_str());
                let marker = populate_messages(&ui.msg_container, &msgs, is_group, anchor);
                *ui.unread_marker_shown.borrow_mut() = marker.is_some();
                *ui.unread_marker.borrow_mut() = marker.clone();
                ui.update_unread_pill();

                let to = match &marker {
                    Some(w) => ScrollTo::Widget(w.clone()),
                    None => ScrollTo::Bottom,
                };
                ui.scroll_to(to);
                // The divider has done its job — showed where you left off and
                // scrolled there. Dismiss it shortly so it doesn't linger over
                // messages you've now read.
                ui.arm_unread_dismiss();
                if SEND_READ_RECEIPTS {
                    if let (Some(guid), Some(handle)) = (receipt_guid, my_handle) {
                        backend.send_receipt(&client, &chat_ref, &handle, true, guid);
                    }
                }
                // The chat is now read; refresh the sidebar to clear its badge.
                ui.reload_chats();
            },
        );
    }

    /// Reload the open chat's messages in place (after sends/receives while
    /// viewing). Follows the bottom only if already there; otherwise holds
    /// position so reading history isn't interrupted.
    fn reload_messages(&self, chat_id: i64, is_group: bool) {
        let store = self.store.clone();
        let ui = self.clone();
        // Rebuild only the window currently loaded (oldest shown -> now), so a
        // new message doesn't collapse history the user scrolled up to read.
        let since = *self.page_oldest.borrow();
        // While the window is backgrounded, recompute the unread boundary so the
        // "New Messages" divider appears live — you can glance at the background
        // window and see it. While focused, keep the existing anchor: the chat is
        // being read and the divider self-dismisses.
        let recompute_unread = !self.focused.get();
        gtk_bridge::spawn(
            async move {
                let msgs = store.messages_from(chat_id, since).await;
                let first = if recompute_unread {
                    Some(store.first_unread_incoming(chat_id).await.ok().flatten())
                } else {
                    None
                };
                (msgs, first)
            },
            move |(res, first)| {
                let msgs = res.unwrap_or_else(|e| {
                    eprintln!("messages load error: {e:#}");
                    Vec::new()
                });
                if let Some(first) = first {
                    *ui.unread.borrow_mut() = first;
                }
                let adj = ui.scroller.vadjustment();
                let at_bottom = adj.value() + adj.page_size() >= adj.upper() - 80.0;
                let prev = adj.value();
                let anchor = ui.unread.borrow().as_ref().map(|(g, _)| g.clone());
                let marker = populate_messages(
                    &ui.msg_container,
                    &msgs,
                    is_group,
                    anchor.as_deref(),
                );
                *ui.unread_marker_shown.borrow_mut() = marker.is_some();
                *ui.unread_marker.borrow_mut() = marker.clone();
                ui.update_unread_pill();
                let to = if at_bottom {
                    ScrollTo::Bottom
                } else {
                    ScrollTo::Value(prev)
                };
                ui.scroll_to(to);
            },
        );
    }

    /// Fetch the page just before the oldest currently-shown message, prepend it,
    /// and keep the viewport anchored on the same message.
    fn maybe_load_older(&self) {
        if *self.page_loading.borrow() || !*self.page_has_more.borrow() {
            return;
        }
        let chat = match self.open_summary.borrow().clone() {
            Some(c) => c,
            None => return,
        };
        let cursor = match *self.page_oldest.borrow() {
            Some(c) => c,
            None => return,
        };
        *self.page_loading.borrow_mut() = true;

        let store = self.store.clone();
        let ui = self.clone();
        let chat_id = chat.id;
        let is_group = chat.is_group;

        gtk_bridge::spawn(
            async move { store.messages_page(chat_id, Some(cursor), PAGE_SIZE).await },
            move |res| {
                let older = res.unwrap_or_default();
                // Bail if the user switched chats while we were loading.
                let still_open = ui
                    .open_summary
                    .borrow()
                    .as_ref()
                    .map_or(false, |c| c.id == chat_id);
                if !still_open {
                    *ui.page_loading.borrow_mut() = false;
                    return;
                }
                if older.is_empty() {
                    *ui.page_has_more.borrow_mut() = false;
                    *ui.page_loading.borrow_mut() = false;
                    return;
                }

                *ui.page_oldest.borrow_mut() = older.first().map(|m| (m.date, m.id));
                *ui.page_has_more.borrow_mut() = older.len() as i64 >= PAGE_SIZE;

                // Capture the anchor right before we change the height — not
                // before the async load, since the user may have scrolled while
                // it was in flight.
                let adj = ui.scroller.vadjustment();
                let old_upper = adj.upper();
                let old_value = adj.value();

                // Prepend in reverse so the batch keeps its order at the top. If
                // this page contains the first unread, the divider slots in here
                // and the floating pill is dismissed.
                let anchor = ui.unread.borrow().as_ref().map(|(g, _)| g.clone());
                let (widgets, marker) =
                    build_message_widgets(&older, is_group, anchor.as_deref());
                for w in widgets.into_iter().rev() {
                    ui.msg_container.prepend(&w);
                }
                if marker.is_some() {
                    *ui.unread_marker_shown.borrow_mut() = true;
                    ui.update_unread_pill();
                }

                // Re-anchor before the frame paints, instead of after a timeout.
                // The tick runs in the update phase and container.measure() already
                // reflects the prepended batch, so we shift the view down by exactly
                // the height we added in the same frame — and stop as soon as it's
                // applied, so we don't fight the user's scroll. The old 50ms timeout
                // let the scrolledwindow paint a few frames at the stale offset
                // first: that was the "above batch" flash on scroll-up.
                let scroller = ui.scroller.clone();
                let container = ui.msg_container.clone();
                let loading = ui.page_loading.clone();
                let frames = Cell::new(0u32);
                ui.scroller.add_tick_callback(move |_w, _clock| {
                    let adj = scroller.vadjustment();
                    let width = container.width();
                    let content_h = if width > 0 {
                        container.measure(gtk::Orientation::Vertical, width).1 as f64
                    } else {
                        adj.upper()
                    };
                    let added = content_h - old_upper;
                    frames.set(frames.get() + 1);
                    // Wait until the prepended batch is reflected in the measure
                    // (or give up after a few frames), then apply the shift once.
                    if added <= 0.5 && frames.get() < 8 {
                        return glib::ControlFlow::Continue;
                    }
                    if content_h > adj.upper() {
                        adj.set_upper(content_h);
                    }
                    adj.set_value(old_value + added.max(0.0));
                    *loading.borrow_mut() = false;
                    glib::ControlFlow::Break
                });
            },
        );
    }

    /// Load everything from the first unread message down to now, render with the
    /// divider in place, and scroll to it. Backs the floating pill.
    fn jump_to_first_unread(&self) {
        let chat = match self.open_summary.borrow().clone() {
            Some(c) => c,
            None => return,
        };
        let (guid, date) = match self.unread.borrow().clone() {
            Some(u) => u,
            None => return,
        };
        let store = self.store.clone();
        let ui = self.clone();
        let chat_id = chat.id;
        let is_group = chat.is_group;
        gtk_bridge::spawn(
            async move { store.messages_from(chat_id, Some((date, 0))).await },
            move |res| {
                let msgs = res.unwrap_or_default();
                let still_open = ui
                    .open_summary
                    .borrow()
                    .as_ref()
                    .map_or(false, |c| c.id == chat_id);
                if !still_open {
                    return;
                }
                *ui.page_oldest.borrow_mut() = msgs.first().map(|m| (m.date, m.id));
                // Read history still sits above the first unread.
                *ui.page_has_more.borrow_mut() = true;
                *ui.page_loading.borrow_mut() = false;
                let marker =
                    populate_messages(&ui.msg_container, &msgs, is_group, Some(guid.as_str()));
                *ui.unread_marker_shown.borrow_mut() = marker.is_some();
                *ui.unread_marker.borrow_mut() = marker.clone();
                ui.update_unread_pill();
                let to = match &marker {
                    Some(w) => ScrollTo::Widget(w.clone()),
                    None => ScrollTo::Bottom,
                };
                ui.scroll_to(to);
            },
        );
    }

    /// Show the pill only when unread messages exist that aren't yet on screen.
    fn update_unread_pill(&self) {
        let show = self.unread.borrow().is_some() && !*self.unread_marker_shown.borrow();
        self.unread_pill.set_visible(show);
    }

    /// Remove the "New Messages" divider and forget the unread boundary, so a
    /// later refresh won't redraw it. Safe to call repeatedly.
    fn dismiss_unread_divider(&self) {
        self.unread_dismiss_gen
            .set(self.unread_dismiss_gen.get().wrapping_add(1));
        if let Some(w) = self.unread_marker.borrow_mut().take() {
            self.msg_container.remove(&w);
        }
        *self.unread.borrow_mut() = None;
        *self.unread_marker_shown.borrow_mut() = false;
        self.update_unread_pill();
    }

    /// Arm a one-shot timer that dismisses the divider after a short dwell.
    /// Always bumps the generation (invalidating any timer from a previously
    /// opened chat); only schedules when a divider is actually on screen.
    fn arm_unread_dismiss(&self) {
        let gen = self.unread_dismiss_gen.get().wrapping_add(1);
        self.unread_dismiss_gen.set(gen);
        if self.unread_marker.borrow().is_none() {
            return;
        }
        let ui = self.clone();
        glib::timeout_add_local_once(
            std::time::Duration::from_secs(UNREAD_DIVIDER_TTL_SECS),
            move || {
                if ui.unread_dismiss_gen.get() == gen {
                    ui.dismiss_unread_divider();
                }
            },
        );
    }

    fn compose_send(&self, entry: &gtk::Entry) {
        let text = entry.text().to_string();
        if text.trim().is_empty() {
            return;
        }
        let Some(chat) = self.open_summary.borrow().clone() else {
            return;
        };
        entry.set_text("");
        self.send_text(&chat, text);
    }

    fn send_text(&self, chat: &ChatSummary, text: String) {
        let Some(my_handle) = self_handle(&chat.participants, &self.handles) else {
            eprintln!("no self handle in chat; cannot send");
            return;
        };
        let chat_ref = chat_ref_of(chat);
        let guid = new_guid();
        let chat_id = chat.id;
        let is_group = chat.is_group;

        // Optimistic record: persist + show the bubble now, before the network
        // round-trip. The real send reuses this guid, so its echo dedupes.
        let optimistic = IncomingMessage {
            guid: guid.clone(),
            chat: chat_ref.clone(),
            sender: Some(my_handle.clone()),
            is_from_me: true,
            text: Some(text.clone()),
            service: Some("iMessage".into()),
            date: now_ms(),
            ..Default::default()
        };

        let store = self.store.clone();
        let backend = self.backend.clone();
        let client = self.client.clone();
        let ui = self.clone();
        gtk_bridge::spawn(
            async move { store.apply(Ingest::Message(optimistic)).await },
            move |res| {
                if let Err(e) = res {
                    eprintln!("optimistic insert failed: {e:#}");
                }
                ui.reload_messages(chat_id, is_group);
                ui.reload_chats();
                // Fire the network send in the background. The optimistic row
                // already carries the final guid, so the echo dedupes and there
                // is nothing to re-render on completion — avoiding a redundant
                // rebuild (and the scroll stutter it caused) a beat after send.
                gtk_bridge::spawn(
                    async move {
                        backend
                            .send_text(&client, &chat_ref, &my_handle, text, guid)
                            .await?;
                        Ok::<(), anyhow::Error>(())
                    },
                    move |res| {
                        if let Err(e) = res {
                            eprintln!("send failed: {e:#}");
                        }
                    },
                );
            },
        );
    }

    fn send_file(&self, path: std::path::PathBuf) {
        let Some(chat) = self.open_summary.borrow().clone() else {
            return;
        };
        let Some(my_handle) = self_handle(&chat.participants, &self.handles) else {
            eprintln!("no self handle in chat; cannot send");
            return;
        };
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "file".to_string());
        let mime = guess_mime(&name);
        let path_str = path.to_string_lossy().into_owned();
        let chat_ref = chat_ref_of(&chat);
        let guid = new_guid();
        let chat_id = chat.id;
        let is_group = chat.is_group;

        // Optimistic record points at the chosen file so the image renders now.
        let optimistic = IncomingMessage {
            guid: guid.clone(),
            chat: chat_ref.clone(),
            sender: Some(my_handle.clone()),
            is_from_me: true,
            service: Some("iMessage".into()),
            date: now_ms(),
            attachments: vec![AttachmentRecord {
                guid: Some(format!("{guid}-0")),
                mime: Some(mime.clone()),
                name: Some(name.clone()),
                local_path: Some(path_str.clone()),
                part_index: Some(0),
                ..Default::default()
            }],
            ..Default::default()
        };

        let store = self.store.clone();
        let backend = self.backend.clone();
        let client = self.client.clone();
        let connection = self.connection.clone();
        let ui = self.clone();
        gtk_bridge::spawn(
            async move { store.apply(Ingest::Message(optimistic)).await },
            move |res| {
                if let Err(e) = res {
                    eprintln!("optimistic insert failed: {e:#}");
                }
                ui.reload_messages(chat_id, is_group);
                ui.reload_chats();
                gtk_bridge::spawn(
                    async move {
                        backend
                            .send_attachment(
                                &client, &connection, &chat_ref, &my_handle, path_str, mime,
                                name, guid,
                            )
                            .await?;
                        Ok::<(), anyhow::Error>(())
                    },
                    move |res| {
                        if let Err(e) = res {
                            eprintln!("attachment send failed: {e:#}");
                        }
                    },
                );
            },
        );
    }

    /// Ack the newest unread inbound message (implicitly marking earlier ones read).
    fn maybe_send_read(&self, chat: &ChatSummary) {
        if !SEND_READ_RECEIPTS {
            return;
        }
        let Some(my_handle) = self_handle(&chat.participants, &self.handles) else {
            return;
        };
        let chat_ref = chat_ref_of(chat);
        let chat_id = chat.id;
        let store = self.store.clone();
        let backend = self.backend.clone();
        let client = self.client.clone();
        let ui = self.clone();
        gtk_bridge::spawn(
            async move {
                match store.latest_unread_incoming(chat_id).await {
                    Ok(Some((guid, date))) => {
                        let _ = store.mark_read_through(chat_id, date).await;
                        Some(guid)
                    }
                    _ => None,
                }
            },
            move |guid| {
                if let Some(guid) = guid {
                    backend.send_receipt(&client, &chat_ref, &my_handle, true, guid);
                    // Something was just marked read; clear its sidebar badge.
                    ui.reload_chats();
                }
            },
        );
    }

    /// Coalesce refresh pulses: a burst of inbound messages (notably the backlog
    /// drained on startup) would otherwise rebuild the sidebar once per message,
    /// flickering the hover/selection. Defer and collapse to a single refresh once
    /// the burst settles.
    fn schedule_refresh(&self) {
        let gen = self.refresh_gen.get().wrapping_add(1);
        self.refresh_gen.set(gen);
        let ui = self.clone();
        glib::timeout_add_local_once(std::time::Duration::from_millis(250), move || {
            // Only the most recent pulse in the burst actually refreshes.
            if ui.refresh_gen.get() == gen {
                ui.refresh();
            }
        });
    }

    fn refresh(&self) {
        self.reload_chats();
        self.process_notifications();
        let open = self.open_summary.borrow().clone();
        if let Some(chat) = open {
            self.reload_messages(chat.id, chat.is_group);
            // Only mark read while we're actually being looked at. Messages that
            // land while the window is in the background stay unread, so the user
            // gets the "new messages" divider when they come back.
            if self.focused.get() {
                self.maybe_send_read(&chat);
            }
        }
    }

    /// Raise a desktop notification for each chat that received new messages,
    /// unless that chat is the one currently open *and* focused. Coalesces per
    /// chat (id-keyed), so new messages replace the prior notification rather
    /// than stacking, and a watermark ensures each message notifies once.
    fn process_notifications(&self) {
        let store = self.store.clone();
        let ui = self.clone();
        let since = self.notify_watermark.get();
        gtk_bridge::spawn(
            async move { store.incoming_since(since).await },
            move |res| {
                let rows = match res {
                    Ok(r) if !r.is_empty() => r,
                    _ => return,
                };
                let mut max_date = ui.notify_watermark.get();
                let mut order: Vec<i64> = Vec::new();
                let mut per_chat: std::collections::HashMap<i64, (String, String, usize)> =
                    std::collections::HashMap::new();
                for m in &rows {
                    max_date = max_date.max(m.date);
                    let preview = m
                        .text
                        .as_deref()
                        .map(strip_marker)
                        .filter(|t| !t.is_empty())
                        .unwrap_or_else(|| {
                            if m.has_attachment {
                                "Sent an attachment".to_string()
                            } else {
                                String::new()
                            }
                        });
                    let sender = m.sender.clone().unwrap_or_default();
                    let e = per_chat.entry(m.chat_id).or_insert_with(|| {
                        order.push(m.chat_id);
                        (String::new(), String::new(), 0)
                    });
                    e.0 = sender;
                    e.1 = preview;
                    e.2 += 1;
                }
                ui.notify_watermark.set(max_date);

                let open_id = ui.open_summary.borrow().as_ref().map(|c| c.id);
                let focused = ui.focused.get();
                for chat_id in order {
                    let (sender, preview, count) = per_chat.remove(&chat_id).unwrap();
                    // Don't notify for the chat the user is actively viewing.
                    if focused && open_id == Some(chat_id) {
                        ui.withdraw_chat_notification(chat_id);
                        continue;
                    }
                    let summary = ui.chats.borrow().iter().find(|c| c.id == chat_id).cloned();
                    let (title, is_group) = match &summary {
                        Some(c) => (chat_title(c, &ui.handles), c.is_group),
                        None => (pretty_addr(&sender), false),
                    };
                    let mut body = if is_group && !sender.is_empty() {
                        format!("{}: {}", pretty_addr(&sender), preview)
                    } else {
                        preview
                    };
                    if count > 1 {
                        body = format!("{body} (+{} earlier)", count - 1);
                    }
                    ui.show_chat_notification(chat_id, &title, &body);
                }
            },
        );
    }

    fn show_chat_notification(&self, chat_id: i64, title: &str, body: &str) {
        let Some(app) = gtk::gio::Application::default() else {
            return;
        };
        let n = gtk::gio::Notification::new(title);
        if !body.is_empty() {
            n.set_body(Some(body));
        }
        n.set_default_action_and_target_value("app.open-chat", Some(&chat_id.to_variant()));
        app.send_notification(Some(&format!("chat-{chat_id}")), &n);
        self.notified_chats.borrow_mut().insert(chat_id);
    }

    fn withdraw_chat_notification(&self, chat_id: i64) {
        if let Some(app) = gtk::gio::Application::default() {
            app.withdraw_notification(&format!("chat-{chat_id}"));
        }
        self.notified_chats.borrow_mut().remove(&chat_id);
    }

    /// Open the chat a clicked notification targets, raising the window first.
    fn activate_chat(&self, chat_id: i64) {
        if let Some(win) = self.window.borrow().as_ref() {
            win.present();
        }
        let summary = self.chats.borrow().iter().find(|c| c.id == chat_id).cloned();
        if let Some(c) = summary {
            self.open_chat(&c);
        }
    }

    /// On regaining focus, if the open chat picked up unread messages while we
    /// were away, re-show it with the unread divider/pill and mark it read —
    /// reusing the same flow as opening the chat fresh.
    fn on_window_focus(&self) {
        let Some(chat) = self.open_summary.borrow().clone() else {
            return;
        };
        let store = self.store.clone();
        let ui = self.clone();
        let chat_id = chat.id;
        gtk_bridge::spawn(
            async move { store.first_unread_incoming(chat_id).await.ok().flatten() },
            move |first| {
                if first.is_some() {
                    // The divider is already on screen from the background
                    // refresh, so just mark the chat read and let it self-dismiss.
                    // No repopulate here — that's what was causing the flicker.
                    ui.maybe_send_read(&chat);
                    ui.arm_unread_dismiss();
                } else {
                    // Already read (e.g. on another device): clear any divider
                    // still lingering from that session.
                    ui.dismiss_unread_divider();
                }
            },
        );
    }

    /// Scroll the timeline to `to` after a rebuild, reliably. The content height
    /// settles over several allocation passes, and setting the adjustment during
    /// those passes gets overridden by GtkScrolledWindow. So instead we re-assert
    /// the target on the frame clock (post-layout) until the height stops changing
    /// — which fixes "opens a notch above the last message until you nudge it".
    fn scroll_to(&self, to: ScrollTo) {
        // Suppress older-page loads from the rebuild's transient scroll resets.
        self.settling.set(true);
        let gen = self.settle_gen.get().wrapping_add(1);
        self.settle_gen.set(gen);
        {
            let settling = self.settling.clone();
            let settle_gen = self.settle_gen.clone();
            glib::timeout_add_local_once(std::time::Duration::from_millis(250), move || {
                if settle_gen.get() == gen {
                    settling.set(false);
                }
            });
        }

        let scroller = self.scroller.clone();
        let container = self.msg_container.clone();
        let frames = Cell::new(0u32);
        let stable = Cell::new(0u32);
        let last_h = Cell::new(f64::NAN);
        self.scroller.add_tick_callback(move |_w, _clock| {
            let adj = scroller.vadjustment();
            let page = adj.page_size();
            // The tick runs in the frame's update phase, before layout, so on the
            // first tick after a rebuild adj.upper() is still the *old* chat's
            // height — targeting it flashes the previous scroll position. Measure
            // the container instead (recomputes for the new content immediately),
            // so the first painted frame already sits at the right place.
            let width = container.width();
            let content_h = if width > 0 {
                container.measure(gtk::Orientation::Vertical, width).1 as f64
            } else {
                adj.upper()
            };
            let bottom = (content_h - page).max(0.0);
            let value = match &to {
                ScrollTo::Bottom => bottom,
                ScrollTo::Value(v) => v.min(bottom),
                ScrollTo::Widget(w) => w
                    .compute_bounds(&container)
                    .map(|b| (b.y() as f64 - 8.0).max(0.0))
                    .unwrap_or(bottom),
            };
            // Push the upper to the measured height first; otherwise set_value is
            // clamped against the stale (pre-layout) upper and lands short. Layout
            // will set the same upper a moment later, so this just wins the frame.
            if content_h > adj.upper() {
                adj.set_upper(content_h);
            }
            adj.set_value(value);

            // Stop once the height has been stable for a few frames (settled), or
            // after a hard cap so we never re-assert indefinitely.
            if (content_h - last_h.get()).abs() < 0.5 {
                stable.set(stable.get() + 1);
            } else {
                stable.set(0);
            }
            last_h.set(content_h);
            frames.set(frames.get() + 1);
            if stable.get() >= 4 || frames.get() >= 24 {
                glib::ControlFlow::Break
            } else {
                glib::ControlFlow::Continue
            }
        });
    }
}

/// Build the row widgets for a message slice with intra-slice grouping/spacing.
/// Inserts the "new messages" divider before the message whose guid matches
/// `unread_anchor` (if present in this slice). No receipt indicator. Used to
/// prepend an older page; returns the divider widget if it landed here.
fn build_message_widgets(
    msgs: &[StoredMessage],
    is_group: bool,
    unread_anchor: Option<&str>,
) -> (Vec<gtk::Widget>, Option<gtk::Widget>) {
    let mut out = Vec::with_capacity(msgs.len());
    let mut marker: Option<gtk::Widget> = None;
    let mut last_key: Option<String> = None;
    let mut last_date = 0i64;
    let mut last_from_me: Option<bool> = None;
    for m in msgs {
        if marker.is_none() && unread_anchor == Some(m.guid.as_str()) {
            let mk = unread_marker();
            out.push(mk.clone());
            marker = Some(mk);
            last_key = None;
            last_from_me = None;
        }
        let key = group_key(m);
        let show_header =
            last_key.as_deref() != Some(key.as_str()) || m.date - last_date > GROUP_GAP_MS;
        let side_changed = last_from_me != Some(m.is_from_me);
        let top = if last_from_me.is_none() {
            8
        } else if side_changed {
            16
        } else if show_header {
            8
        } else {
            2
        };
        out.push(message_widget(m, show_header, is_group, top));
        last_key = Some(key);
        last_date = m.date;
        last_from_me = Some(m.is_from_me);
    }
    (out, marker)
}

fn populate_messages(
    container: &gtk::Box,
    msgs: &[StoredMessage],
    is_group: bool,
    unread_anchor: Option<&str>,
) -> Option<gtk::Widget> {
    clear_box(container);
    let mut last_key: Option<String> = None;
    let mut last_date = 0i64;
    let mut last_from_me: Option<bool> = None;
    let mut marker: Option<gtk::Widget> = None;
    // The single message that carries the Delivered/Read indicator.
    let last_sent_idx = msgs.iter().rposition(|m| m.is_from_me);

    for (i, m) in msgs.iter().enumerate() {
        // Place the "new messages" divider immediately before the exact first
        // unread message (matched by guid), so it can't drift to the top of a
        // partially-loaded window.
        if marker.is_none() && unread_anchor == Some(m.guid.as_str()) {
            let mk = unread_marker();
            container.append(&mk);
            marker = Some(mk);
            // Start the unread run with a fresh header.
            last_key = None;
            last_from_me = None;
        }

        let key = group_key(m);
        let show_header =
            last_key.as_deref() != Some(key.as_str()) || m.date - last_date > GROUP_GAP_MS;
        // Bigger gap on a received <-> sent flip, medium for a new same-side
        // group, tight within a group.
        let side_changed = last_from_me != Some(m.is_from_me);
        let top = if last_from_me.is_none() {
            8
        } else if side_changed {
            16
        } else if show_header {
            8
        } else {
            2
        };
        container.append(&message_widget(m, show_header, is_group, top));

        // Delivered/Read indicator: only under the most recent sent message, so
        // it moves forward as new messages are sent and never lingers on older ones.
        if Some(i) == last_sent_idx {
            match receipt_status(m) {
                Some(status) => container.append(&receipt_label(&status)),
                // When the freshly sent message is at the very bottom, reserve the
                // receipt line ahead of time (an invisible, same-height placeholder)
                // so the bubble doesn't bump up the moment "Delivered" arrives.
                None if i == msgs.len() - 1 => {
                    container.append(&receipt_label("\u{200b}"))
                }
                None => {}
            }
        }

        last_key = Some(key);
        last_date = m.date;
        last_from_me = Some(m.is_from_me);
    }
    marker
}

/// "Read 16:06" if read, else "Delivered" if delivered, else nothing.
fn receipt_status(m: &StoredMessage) -> Option<String> {
    if let Some(d) = m.date_read {
        Some(format!("Read {}", fmt_time(d)))
    } else if m.date_delivered.is_some() {
        Some("Delivered".to_string())
    } else {
        None
    }
}

fn receipt_label(text: &str) -> gtk::Widget {
    let l = gtk::Label::builder()
        .label(text)
        .halign(gtk::Align::End)
        .margin_end(16)
        .margin_top(1)
        .margin_bottom(4)
        .build();
    l.add_css_class("dim-label");
    l.add_css_class("caption");
    l.upcast()
}

/// A centered "New messages" divider with hairlines on each side.
fn unread_marker() -> gtk::Widget {
    let row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .margin_start(14)
        .margin_end(14)
        .margin_top(10)
        .margin_bottom(2)
        .build();
    let left = gtk::Separator::new(gtk::Orientation::Horizontal);
    left.set_hexpand(true);
    left.set_valign(gtk::Align::Center);
    let lbl = gtk::Label::new(Some("New messages"));
    lbl.add_css_class("unread-marker");
    let right = gtk::Separator::new(gtk::Orientation::Horizontal);
    right.set_hexpand(true);
    right.set_valign(gtk::Align::Center);
    row.append(&left);
    row.append(&lbl);
    row.append(&right);
    row.upcast()
}

/// Where to land the timeline after (re)populating.
enum ScrollTo {
    Bottom,
    Value(f64),
    Widget(gtk::Widget),
}

/// A sidebar row: avatar + chat name + unread badge.
fn chat_row(c: &ChatSummary, handles: &[String]) -> adw::ActionRow {
    let title = chat_title(c, handles);
    let row = adw::ActionRow::builder().title(&title).build();
    row.set_activatable(true);

    let avatar = adw::Avatar::new(36, Some(&title), true);
    row.add_prefix(&avatar);

    if c.unread > 0 {
        let badge = gtk::Label::new(Some(&c.unread.to_string()));
        badge.add_css_class("unread-badge");
        badge.set_valign(gtk::Align::Center);
        row.add_suffix(&badge);
    }
    row
}

/// One message in the timeline. Incoming messages are grey bubbles on the left
/// (with an avatar, and a sender name in group chats); our own messages are blue
/// bubbles on the right.
fn message_widget(m: &StoredMessage, show_header: bool, is_group: bool, top: i32) -> gtk::Widget {
    if m.is_from_me {
        own_message(m, show_header, top)
    } else {
        incoming_message(m, show_header, is_group, top)
    }
}

/// Left: grey bubble, with an avatar + sender name in group chats only.
fn incoming_message(m: &StoredMessage, show_header: bool, is_group: bool, top: i32) -> gtk::Widget {
    let row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .margin_start(14)
        .margin_end(56)
        .margin_top(top)
        .halign(gtk::Align::Start)
        .build();

    // Avatars (and their continuation spacer) only in group chats.
    if is_group {
        if show_header {
            let avatar = adw::Avatar::new(28, Some(&sender_display(m)), true);
            avatar.set_valign(gtk::Align::Start);
            row.append(&avatar);
        } else {
            let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
            spacer.set_size_request(28, -1);
            row.append(&spacer);
        }
    }

    let col = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(2)
        .halign(gtk::Align::Start)
        .build();

    if is_group && show_header {
        let name = gtk::Label::builder()
            .label(sender_display(m))
            .xalign(0.0)
            .build();
        name.add_css_class("sender-name");
        col.append(&name);
    }

    let line = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(6)
        .build();
    line.append(&message_body(m, false));
    if show_header {
        line.append(&time_label(m));
    }
    col.append(&line);

    row.append(&col);
    row.upcast()
}

/// Right: blue bubble, time to its left on the first bubble of a group.
fn own_message(m: &StoredMessage, show_header: bool, top: i32) -> gtk::Widget {
    let row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .margin_start(56)
        .margin_end(14)
        .margin_top(top)
        .halign(gtk::Align::End)
        .build();

    let line = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(6)
        .halign(gtk::Align::End)
        .build();
    if show_header {
        line.append(&time_label(m));
    }
    line.append(&message_body(m, true));

    row.append(&line);
    row.upcast()
}

/// The visual content of a message: image attachments stacked above an optional
/// text bubble, aligned to the sender's side.
fn message_body(m: &StoredMessage, own: bool) -> gtk::Widget {
    let col = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(3)
        .halign(if own {
            gtk::Align::End
        } else {
            gtk::Align::Start
        })
        .build();

    for att in &m.attachments {
        let placed = att
            .is_image()
            .then(|| att.local_path.as_deref())
            .flatten()
            .and_then(image_widget);
        match placed {
            Some(pic) => col.append(&pic),
            None => col.append(&file_chip(att, own)),
        }
    }

    let has_text = m
        .text
        .as_deref()
        .map_or(false, |t| !strip_marker(t).is_empty());
    let is_tapback = m.associated_guid.is_some();
    if has_text || is_tapback {
        let bubble = bubble_box(own);
        bubble.append(&bubble_label(&body_text(m)));
        col.append(&bubble);
    } else if m.attachments.is_empty() {
        let bubble = bubble_box(own);
        bubble.append(&bubble_label("(no text)"));
        col.append(&bubble);
    }

    col.upcast()
}

/// iMessage marks attachment positions in the text stream with U+FFFC; drop it
/// (and surrounding whitespace) so attachment-only messages read as empty.
fn strip_marker(s: &str) -> String {
    s.replace('\u{FFFC}', "").trim().to_string()
}

/// A rounded, size-capped image from a local file, or `None` if it can't load
/// (e.g. an unsupported format like HEIC without a decoder).
fn image_widget(path: &str) -> Option<gtk::Widget> {
    let texture = gtk::gdk::Texture::from_filename(path).ok()?;
    let (iw, ih) = (texture.width() as f64, texture.height() as f64);
    if iw <= 0.0 || ih <= 0.0 {
        return None;
    }
    let (max_w, max_h) = (260.0, 340.0);
    let scale = (max_w / iw).min(max_h / ih).min(1.0);
    let pic = gtk::Picture::new();
    pic.set_paintable(Some(&texture));
    pic.set_size_request((iw * scale).round() as i32, (ih * scale).round() as i32);
    pic.set_content_fit(gtk::ContentFit::Contain);
    pic.set_overflow(gtk::Overflow::Hidden);
    pic.add_css_class("attachment-image");
    pic.set_cursor_from_name(Some("pointer"));

    // Click to enlarge: find the lightbox host overlay and layer the full image.
    let gesture = gtk::GestureClick::new();
    let path_owned = path.to_string();
    let pic_click = pic.clone();
    gesture.connect_released(move |_, _, _, _| {
        if let Some(host) = find_lightbox_host(pic_click.upcast_ref()) {
            show_lightbox(&host, &path_owned);
        }
    });
    pic.add_controller(gesture);

    Some(pic.upcast())
}

/// Walk up from `w` to the named overlay we wrap the messaging UI in.
fn find_lightbox_host(w: &gtk::Widget) -> Option<gtk::Overlay> {
    let mut cur = w.parent();
    while let Some(p) = cur {
        if p.widget_name().as_str() == "lightbox-host" {
            return p.downcast::<gtk::Overlay>().ok();
        }
        cur = p.parent();
    }
    None
}

/// Layer a dimmed, centered, full-size image over the UI. Click anywhere or
/// press Escape to dismiss.
fn show_lightbox(host: &gtk::Overlay, path: &str) {
    let Ok(texture) = gtk::gdk::Texture::from_filename(path) else {
        return;
    };

    let dim = gtk::Box::new(gtk::Orientation::Vertical, 0);
    dim.add_css_class("lightbox-dim");
    dim.set_hexpand(true);
    dim.set_vexpand(true);
    dim.set_focusable(true);

    let pic = gtk::Picture::new();
    pic.set_paintable(Some(&texture));
    pic.set_content_fit(gtk::ContentFit::ScaleDown);
    pic.set_can_shrink(true);
    pic.set_hexpand(true);
    pic.set_vexpand(true);
    pic.set_margin_top(32);
    pic.set_margin_bottom(32);
    pic.set_margin_start(32);
    pic.set_margin_end(32);
    dim.append(&pic);

    // Click anywhere on the dim layer dismisses.
    let click = gtk::GestureClick::new();
    let host_c = host.clone();
    let dim_c = dim.clone();
    click.connect_released(move |_, _, _, _| host_c.remove_overlay(&dim_c));
    dim.add_controller(click);

    // Escape dismisses.
    let keys = gtk::EventControllerKey::new();
    let host_k = host.clone();
    let dim_k = dim.clone();
    keys.connect_key_pressed(move |_, key, _, _| {
        if key == gtk::gdk::Key::Escape {
            host_k.remove_overlay(&dim_k);
            glib::Propagation::Stop
        } else {
            glib::Propagation::Proceed
        }
    });
    dim.add_controller(keys);

    host.add_overlay(&dim);
    dim.grab_focus();
}

/// A bubble with a file icon + name, for non-image (or undecodable) attachments.
fn file_chip(att: &StoredAttachment, own: bool) -> gtk::Widget {
    let bubble = bubble_box(own);
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    row.append(&gtk::Image::from_icon_name("text-x-generic-symbolic"));
    let name = att.name.clone().unwrap_or_else(|| "Attachment".to_string());
    row.append(&gtk::Label::new(Some(&name)));
    bubble.append(&row);
    bubble.upcast()
}

/// A rounded bubble container; `own` selects blue-on-white vs grey-on-dark.
fn bubble_box(own: bool) -> gtk::Box {
    let b = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    b.add_css_class("bubble");
    b.add_css_class(if own { "bubble-out" } else { "bubble-in" });
    b
}

/// The wrapped, width-capped, left-justified text inside a bubble.
fn bubble_label(text: &str) -> gtk::Label {
    gtk::Label::builder()
        .label(text)
        .wrap(true)
        .xalign(0.0)
        .selectable(true)
        .max_width_chars(40)
        .build()
}

/// A small dim timestamp aligned to the bottom of a bubble.
fn time_label(m: &StoredMessage) -> gtk::Label {
    let l = gtk::Label::builder().label(fmt_time(m.date)).build();
    l.add_css_class("dim-label");
    l.add_css_class("caption");
    l.set_valign(gtk::Align::End);
    l
}

// --- scaffolding helpers ---

/// A toolbar-view page: header with `title`, `body` as content, optional bottom bar.
fn page(
    title: &str,
    body: &impl IsA<gtk::Widget>,
    bottom: Option<&gtk::Widget>,
) -> adw::NavigationPage {
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());
    toolbar.set_content(Some(body));
    if let Some(b) = bottom {
        toolbar.add_bottom_bar(b);
    }
    adw::NavigationPage::builder()
        .title(title)
        .child(&toolbar)
        .build()
}

fn scrolled(child: &impl IsA<gtk::Widget>) -> gtk::ScrolledWindow {
    gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(child)
        .build()
}

fn guess_mime(name: &str) -> String {
    let ext = name.rsplit('.').next().unwrap_or("").to_lowercase();
    match ext.as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "heic" | "heif" => "image/heic",
        "webp" => "image/webp",
        "mp4" => "video/mp4",
        "mov" => "video/quicktime",
        "pdf" => "application/pdf",
        _ => "application/octet-stream",
    }
    .to_string()
}

fn install_css() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let provider = gtk::CssProvider::new();
        provider.load_from_string(CSS);
        if let Some(display) = gtk::gdk::Display::default() {
            gtk::style_context_add_provider_for_display(
                &display,
                &provider,
                gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
            );
            // Bundled symbolic icons (e.g. the send arrow). Baked-in absolute
            // path so it resolves regardless of the working directory in dev.
            let theme = gtk::IconTheme::for_display(&display);
            theme.add_search_path(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/icons"));
        }
    });
}

// --- formatting helpers ---

fn chat_title(c: &ChatSummary, handles: &[String]) -> String {
    if let Some(n) = &c.display_name {
        if !n.is_empty() {
            return n.clone();
        }
    }
    let is_me = |p: &str| handles.iter().any(|h| h.as_str().eq_ignore_ascii_case(p));
    let others: Vec<String> = c
        .participants
        .iter()
        .filter(|p| !is_me(p.as_str()))
        .map(|p| pretty_addr(p))
        .collect();
    if !others.is_empty() {
        return others.join(", ");
    }
    // Note-to-self (only our own handle) or empty: show what we have.
    let all: Vec<String> = c.participants.iter().map(|p| pretty_addr(p)).collect();
    if all.is_empty() {
        c.key.clone()
    } else {
        all.join(", ")
    }
}

fn sender_display(m: &StoredMessage) -> String {
    if m.is_from_me {
        "You".to_string()
    } else {
        m.sender
            .as_deref()
            .map(pretty_addr)
            .unwrap_or_else(|| "Unknown".to_string())
    }
}

/// An iMessage-style guid (uppercased UUID v4) for optimistic local inserts.
fn new_guid() -> String {
    glib::uuid_string_random().to_string().to_uppercase()
}

/// Unix epoch milliseconds, matching the backend's message timestamps.
fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn group_key(m: &StoredMessage) -> String {
    if m.is_from_me {
        "\0me".to_string()
    } else {
        m.sender.clone().unwrap_or_default()
    }
}

fn body_text(m: &StoredMessage) -> String {
    match (&m.text, &m.associated_guid) {
        (Some(t), _) if !strip_marker(t).is_empty() => strip_marker(t),
        (_, Some(_)) => format!("reacted ({}) to a message", m.associated_type.unwrap_or(0)),
        _ => "(no text)".to_string(),
    }
}

fn fmt_time(ms: i64) -> String {
    glib::DateTime::from_unix_local(ms / 1000)
        .and_then(|dt| dt.format("%H:%M"))
        .map(|s| s.to_string())
        .unwrap_or_default()
}

fn pretty_addr(a: &str) -> String {
    a.strip_prefix("mailto:")
        .or_else(|| a.strip_prefix("tel:"))
        .unwrap_or(a)
        .to_string()
}

/// Our own address within a conversation, used as the sender for outbound items.
fn self_handle(participants: &[String], handles: &[String]) -> Option<String> {
    participants
        .iter()
        .find(|p| {
            handles
                .iter()
                .any(|h| h.as_str().eq_ignore_ascii_case(p.as_str()))
        })
        .cloned()
}

fn chat_ref_of(c: &ChatSummary) -> ChatRef {
    ChatRef {
        participants: c.participants.clone(),
        display_name: c.display_name.clone(),
        service: c.service.clone(),
    }
}

fn clear(list: &gtk::ListBox) {
    while let Some(child) = list.first_child() {
        list.remove(&child);
    }
}

fn clear_box(b: &gtk::Box) {
    while let Some(child) = b.first_child() {
        b.remove(&child);
    }
}
