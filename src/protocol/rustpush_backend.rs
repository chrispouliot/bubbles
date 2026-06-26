//! The real backend: implements [`Backend`] over `rustpush` + the vendored
//! `api.rs` subset (exposed as `crate::api`). Where [`stub`](super::stub) is
//! a no-op for offline iteration, this is the full Apple-stack path: Apple
//! ID login (NAC validation goes through the locally-vendored
//! `open-absinthe` crate), 2FA via SMS or trusted device, message
//! send/receive, tapbacks, attachments, link previews.
//!
//! Gated by the `rustpush` Cargo feature, which is in the workspace's
//! `default` set, so this module compiles in normal `cargo build` runs.
//! `main` constructs the corresponding `Arc<dyn Backend>` at startup;
//! `--no-default-features` drops the rustpush dep entirely and falls back
//! to [`stub`](super::stub).
//!
//! Handle mapping (our opaque handle <- concrete type):
//!   Config        <- api::JoinedOSConfig
//!   Connection    <- ConnHandle { conn: APSConnection, idms: Arc<IdmsAuthListener> }
//!   Anisette      <- ArcAnisetteClient<DefaultAnisetteProvider>
//!   Identity      <- IDSNGMIdentity
//!   Account       <- Arc<Mutex<AppleAccount<DefaultAnisetteProvider>>>
//!   IdsUser       <- IDSUser
//!   CircleSession <- CircleHandle { session+watcher behind a Mutex, + idms }
//!   VerifyBody    <- rustpush::VerifyBody
//!   ImClient      <- Arc<IMClient>

use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::{broadcast, Mutex};

use rustpush::{
    APSConnection, APSMessage, AppleAccount, ArcAnisetteClient, Attachment,
    CircleClientSession, ConversationData, DebugMutex, DefaultAnisetteProvider, IDSNGMIdentity,
    IDSUser, IMClient, IdmsAuthListener, IndexedMessagePart, LPLinkMetadata, LinkMeta,
    LoginState as RpLoginState, MMCSFile, Message, MessageInst, MessagePart, MessageParts,
    MessageType, NormalMessage, ReactMessage, Reaction, ReactMessageType, VerifyBody as RpVerifyBody,
};

use crate::store::{
    AttachmentRecord, ChatRef, IncomingMessage, Ingest, MessageLinkPreview, Receipt, Store,
    Tapback,
};

use crate::api;
use crate::api::buffered_conn::BufferedApsConn;
use crate::protocol::*;

type Anis = ArcAnisetteClient<DefaultAnisetteProvider>;
// api.rs uses `rustpush::DebugMutex as Mutex`, so the account is wrapped in DebugMutex.
type AppleAcct = Arc<DebugMutex<AppleAccount<DefaultAnisetteProvider>>>;

/// Connection + the idms listener created alongside it (needed for 2FA verify).
struct ConnHandle {
    conn: Arc<BufferedApsConn>,
    idms: Arc<IdmsAuthListener>,
}

/// Trusted-device 2FA session: the circle session and the APS watcher subscribed
/// *before* the push was sent, plus the idms listener. All behind one Mutex so
/// `verify_2fa` can take `&mut` to the session and the receiver at once.
struct CircleHandle {
    inner: Mutex<(CircleClientSession<DefaultAnisetteProvider>, broadcast::Receiver<APSMessage>)>,
    idms: Arc<IdmsAuthListener>,
}

pub struct RustpushBackend {
    state_path: String,
}

impl RustpushBackend {
    pub fn new(state_path: impl Into<String>) -> Self {
        // Caller must have run api::do_first_time_init(&state_path) once at boot.
        Self {
            state_path: state_path.into(),
        }
    }
}

// --- handle <-> concrete-type accessors ---

fn cfg(c: &Config) -> &api::JoinedOSConfig {
    c.downcast().expect("Config holds JoinedOSConfig")
}
fn conn(c: &Connection) -> &ConnHandle {
    c.downcast().expect("Connection holds ConnHandle")
}
fn anis(a: &Anisette) -> &Anis {
    a.downcast().expect("Anisette holds ArcAnisetteClient")
}
fn ident(i: &Identity) -> &IDSNGMIdentity {
    i.downcast().expect("Identity holds IDSNGMIdentity")
}
fn acct(a: &Account) -> &AppleAcct {
    a.downcast().expect("Account holds Arc<Mutex<AppleAccount>>")
}
fn circle(c: &CircleSession) -> &CircleHandle {
    c.downcast().expect("CircleSession holds CircleHandle")
}
fn vbody(v: &VerifyBody) -> &RpVerifyBody {
    v.downcast().expect("VerifyBody holds rustpush::VerifyBody")
}
fn client(c: &ImClient) -> &Arc<IMClient> {
    c.downcast().expect("ImClient holds Arc<IMClient>")
}

/// rustpush `LoginState` -> our facade `LoginState`.
fn map_state(s: RpLoginState) -> LoginState {
    match s {
        RpLoginState::LoggedIn => LoginState::LoggedIn,
        RpLoginState::NeedsLogin => LoginState::NeedsLogin,
        RpLoginState::NeedsDevice2FA => LoginState::NeedsDevice2Fa,
        RpLoginState::Needs2FAVerification => LoginState::Needs2FaVerification,
        RpLoginState::NeedsSMS2FA => LoginState::NeedsSms2Fa,
        RpLoginState::NeedsSMS2FAVerification(body) => {
            LoginState::NeedsSms2FaVerification(VerifyBody::new(body))
        }
        RpLoginState::NeedsExtraStep(msg) => LoginState::NeedsExtraStep(msg),
    }
}

#[async_trait]
impl Backend for RustpushBackend {
    // --- 1. hardware token / validation data ---

    async fn config_from_relay(
        &self,
        code: String,
        host: String,
        token: Option<String>,
    ) -> Result<Config> {
        // api signature takes `token: &Option<String>`
        let cfg = api::config_from_relay(code, host, &token).await?;
        Ok(Config::new(cfg))
    }

    async fn config_from_validation_data(&self, data: Vec<u8>, _extra: HwExtra) -> Result<Config> {
        // Standard device-identity values for the raw-validation-data path,
        // matching upstream hw_inp.dart. (Relay path is primary and skips this.)
        let extra = api::HwExtra {
            version: "13.6.4".into(),
            protocol_version: 1660,
            device_id: uuid::Uuid::new_v4().to_string(),
            icloud_ua: "com.apple.iCloudHelper/282 CFNetwork/1408.0.4 Darwin/22.5.0".into(),
            aoskit_version: "com.apple.AOSKit/282 (com.apple.accountsd/113)".into(),
        };
        // config_from_validation_data is synchronous.
        let cfg = api::config_from_validation_data(data, extra)?;
        Ok(Config::new(cfg))
    }

    async fn config_from_encoded(&self, encoded: Vec<u8>) -> Result<Config> {
        // Rehydrate a MacOSConfig from a cached bbhwinfo blob (device_id and
        // version are embedded in the blob, so no HwExtra is needed).
        let cfg = api::config_from_encoded(encoded)?;
        Ok(Config::new(cfg))
    }

    async fn device_info(&self, config: &Config) -> Result<DeviceInfo> {
        let d = api::get_device_info(cfg(config))?;
        Ok(DeviceInfo {
            name: d.name,
            serial: d.serial,
            os_version: d.os_version,
        })
    }

    // --- 2. push + identity + anisette ---

    fn new_identity(&self) -> Result<Identity> {
        Ok(Identity::new(api::new_ngm_identity()?))
    }

    async fn setup_push(&self, config: &Config, identity: &Identity) -> Result<Connection> {
        // `state: None` = fresh connection. For session restore,
        // pass the saved Option<APSState> here instead.
        let (conn, err) =
            api::setup_push(cfg(config), ident(identity), None, self.state_path.clone()).await;
        if let Some(e) = err {
            log::warn!("setup_push returned a non-fatal error: {e:?}");
        }
        let idms = api::make_idms(&conn).await;
        Ok(Connection::new(ConnHandle { conn, idms }))
    }

    async fn make_anisette(&self, config: &Config, connection: &Connection) -> Result<Anisette> {
        let a = api::make_anisette(self.state_path.clone(), cfg(config), conn(connection).conn.inner()).await;
        Ok(Anisette::new(a))
    }

    // --- 3. login + 2FA ---

    async fn try_auth(
        &self,
        config: &Config,
        connection: &Connection,
        anisette: &Anisette,
        creds: Option<(String, String)>,
    ) -> Result<(Account, LoginState)> {
        let (account, state) = api::try_auth(
            self.state_path.clone(),
            cfg(config),
            conn(connection).conn.inner(),
            anis(anisette),
            creds,
        )
        .await?;
        Ok((Account::new(account), map_state(state)))
    }

    async fn try_icloud_login(
        &self,
        config: &Config,
        account: &Account,
    ) -> Result<Option<IdsUser>> {
        let user = api::try_icloud_login(self.state_path.clone(), cfg(config), acct(account)).await?;
        Ok(user.map(IdsUser::new))
    }

    async fn send_2fa_to_devices(
        &self,
        account: &Account,
        connection: &Connection,
    ) -> Result<(CircleSession, LoginState)> {
        let ch = conn(connection);
        // Subscribe BEFORE sending so the verification push isn't missed.
        let watcher = api::subscribe_conn(&ch.conn);
        let (session, state, _sid) = api::send_2fa_to_devices(acct(account), ch.conn.inner()).await?;
        let handle = CircleHandle {
            inner: Mutex::new((session, watcher)),
            idms: ch.idms.clone(),
        };
        Ok((CircleSession::new(handle), map_state(state)))
    }

    async fn verify_2fa(
        &self,
        session: &CircleSession,
        anisette: &Anisette,
        config: &Config,
        account: &Account,
        code: String,
    ) -> Result<(LoginState, Option<IdsUser>)> {
        let ch = circle(session);
        let mut guard = ch.inner.lock().await;
        let (sess, watcher) = &mut *guard;
        let (state, user) = api::verify_2fa(
            self.state_path.clone(),
            sess,
            anis(anisette),
            cfg(config),
            acct(account),
            watcher,
            &ch.idms,
            code,
        )
        .await?;
        Ok((map_state(state), user.map(IdsUser::new)))
    }

    async fn send_2fa_sms(&self, account: &Account) -> Result<LoginState> {
        let (phones, maybe_state) = api::get_2fa_sms_opts(acct(account)).await?;
        if let Some(s) = maybe_state {
            return Ok(map_state(s));
        }
        // VERIFY: this just picks the first trusted number. To match upstream's
        // picker, surface `phones` (TrustedPhoneNumber { id, number_with_dial_code,
        // .. }) to the UI and pass the chosen id. `locked` is the circle session
        // from a prior device-2FA attempt; None is fine for a pure SMS flow.
        let phone_id = phones
            .first()
            .map(|p| p.id)
            .ok_or_else(|| anyhow::anyhow!("no trusted phone numbers"))?;
        let state = api::send_2fa_sms(None, acct(account), phone_id).await?;
        Ok(map_state(state))
    }

    async fn verify_2fa_sms(
        &self,
        account: &Account,
        anisette: &Anisette,
        config: &Config,
        body: &VerifyBody,
        code: String,
    ) -> Result<(LoginState, Option<IdsUser>)> {
        let (state, user) = api::verify_2fa_sms(
            self.state_path.clone(),
            acct(account),
            anis(anisette),
            cfg(config),
            vbody(body),
            code,
        )
        .await?;
        Ok((map_state(state), user.map(IdsUser::new)))
    }

    // --- 4. registration ---

    async fn register_ids(
        &self,
        config: &Config,
        connection: &Connection,
        identity: &Identity,
        users: Vec<IdsUser>,
    ) -> Result<RegisterOutcome> {
        // FRB took ownership via duplicate_user upstream; we mirror that so the
        // handles stay reusable.
        let users_vec: Vec<IDSUser> = users
            .iter()
            .map(|u| api::duplicate_user(u.downcast::<IDSUser>().expect("IdsUser")))
            .collect();

        let (new_users, alert) = api::register_ids(
            self.state_path.clone(),
            cfg(config),
            conn(connection).conn.inner(),
            ident(identity),
            users_vec,
        )
        .await?;

        match alert {
            Some(a) => Ok(RegisterOutcome::Blocked(SupportAlert {
                title: a.title,
                body: a.body,
            })),
            None => Ok(RegisterOutcome::Registered(
                new_users
                    .unwrap_or_default()
                    .into_iter()
                    .map(IdsUser::new)
                    .collect(),
            )),
        }
    }

    async fn make_imclient(
        &self,
        connection: &Connection,
        identity: &Identity,
        users: Vec<IdsUser>,
    ) -> Result<ImClient> {
        let users_vec: Vec<IDSUser> = users
            .iter()
            .map(|u| api::duplicate_user(u.downcast::<IDSUser>().expect("IdsUser")))
            .collect();
        let c = api::make_imclient(
            self.state_path.clone(),
            conn(connection).conn.inner(),
            &users_vec,
            ident(identity),
        )
        .await;
        Ok(ImClient::new(c))
    }

    async fn get_handles(&self, c: &ImClient) -> Result<Vec<String>> {
        Ok(api::get_handles(client(c)).await?)
    }

    async fn restore_session(&self) -> Result<Option<RestoredSession>> {
        let path = self.state_path.clone();

        // hw_info.plist (push + identity + os_config) and id.plist (registered
        // users) are both written during a successful onboarding. Either being
        // absent means we have nothing to restore -> onboard.
        let Some(saved) = api::read_hardware(path.clone()) else {
            return Ok(None);
        };
        let Some(users) = api::restore_users(path.clone()) else {
            return Ok(None);
        };
        if users.is_empty() {
            return Ok(None);
        }

        let identity = api::decode_identity(&saved.identity)?;
        let config = saved.os_config.clone();

        // Reconnect APNs reusing the saved push state (no fresh activation).
        let (conn, err) =
            api::setup_push(&config, &identity, Some(saved.push), path.clone()).await;
        if let Some(e) = err {
            log::warn!("restore setup_push returned a non-fatal error: {e:?}");
        }
        let idms = api::make_idms(&conn).await;

        // Rehydrate the messaging client straight from the persisted
        // registration in id.plist — no re-register, no validation data needed.
        let imclient = api::make_imclient(path.clone(), conn.inner(), &users, &identity).await;
        let handles = api::get_handles(&imclient).await?;

        Ok(Some(RestoredSession {
            config: Config::new(config),
            connection: Connection::new(ConnHandle { conn, idms }),
            identity: Identity::new(identity),
            client: ImClient::new(imclient),
            handles,
        }))
    }

    fn start_receiving(
        &self,
        connection: &Connection,
        c: &ImClient,
        handles: Vec<String>,
        store: Store,
        notify: async_channel::Sender<RecvEvent>,
    ) {
        let conn = conn(connection).conn.clone();
        let imclient = client(c).clone();
        crate::runtime::runtime().spawn(async move {
            let mut rx = api::subscribe_conn(&conn);
            log::info!("receive loop started");
            loop {
                match rx.recv().await {
                    Ok(msg) => match imclient.handle(msg).await {
                        Ok(Some(inst)) => {
                            log_inst(&inst);
                            // Typing is ephemeral: forward it straight to the UI
                            // (keyed like the store's chat key) and don't persist
                            // or acknowledge it.
                            if let Message::Typing(typing, _) = &inst.message {
                                let from_me = is_from_me(&inst, &handles);
                                log::debug!(
                                    "typing recv from={:?} from_me={from_me} typing={typing}",
                                    inst.sender
                                );
                                if !from_me {
                                    if let Some(conv) = &inst.conversation {
                                        let chat_key = ChatRef {
                                            participants: conv.participants.clone(),
                                            display_name: conv.cv_name.clone(),
                                            service: None,
                                        }
                                        .key();
                                        let _ = notify
                                            .send(RecvEvent::Typing {
                                                chat_key,
                                                from: inst.sender.clone(),
                                                typing: *typing,
                                                superseded: false,
                                            })
                                            .await;
                                    }
                                }
                                continue;
                            }
                            let mut ingest = ingest_from(&inst, &handles);
                            // Download any attachments and attach them to the record.
                            if let Ingest::Message(im) = &mut ingest {
                                im.attachments = download_inbound(&inst, conn.inner(), &im.guid).await;
                            }
                            if let Err(e) = store.apply(ingest).await {
                                log::warn!("store apply error: {e:#}");
                            }
                            // Sender-generated link preview (iMessage rich link):
                            // rustpush already pulled the balloon body and gave us
                            // the inline thumbnail bytes; we cache them to disk and
                            // upsert the row. Same guid as the message so a
                            // placeholder is replaced in place by its fill-in.
                            // MUST NOT fetch the URL — that would leak the
                            // recipient's IP to the sender's hosting (a tracking
                            // beacon). The sender already shipped us the snapshot.
                            //
                            // We do NOT pulse RecvEvent::Applied here: a full
                            // `reload_messages` on a preview-only update flickers
                            // and can jump scroll (per the plan). Instead we send
                            // RecvEvent::LinkPreviewUpdated, which the UI handles
                            // as an in-place card replacement.
                            if let Message::Message(nm) = &inst.message {
                                if let Some(lm) = &nm.link_meta {
                                    match extract_link_preview(&inst.id, lm) {
                                        Some(p) => {
                                            if let Err(e) =
                                                store.apply(Ingest::LinkPreview(p)).await
                                            {
                                                log::warn!("store apply link preview: {e:#}");
                                            } else {
                                                let _ = notify
                                                    .send(RecvEvent::LinkPreviewUpdated {
                                                        guid: inst.id.clone(),
                                                        part_idx: 0,
                                                    })
                                                    .await;
                                            }
                                        }
                                        None => log::debug!("link_meta present but no preview extracted for {}", inst.id),
                                    }
                                }
                            }
                            // Acknowledge inbound content with a Delivered receipt.
                            if SEND_DELIVERED_RECEIPTS && is_incoming_content(&inst, &handles) {
                                send_receipt_for(&imclient, &inst, &handles, false).await;
                            }
                            // Pulse the UI; drop if no receiver is listening.
                            let _ = notify.send(RecvEvent::Applied).await;
                            // A real inbound message means they've stopped typing:
                            // clear the indicator now rather than waiting for a
                            // typing-stop that iMessage doesn't always send.
                            if is_incoming_content(&inst, &handles) {
                                if let Some(conv) = &inst.conversation {
                                    let chat_key = ChatRef {
                                        participants: conv.participants.clone(),
                                        display_name: conv.cv_name.clone(),
                                        service: None,
                                    }
                                    .key();
                                    let _ = notify
                                        .send(RecvEvent::Typing {
                                            chat_key,
                                            from: inst.sender.clone(),
                                            typing: false,
                                            superseded: true,
                                        })
                                        .await;
                                }
                            }
                        }
                        Ok(None) => {}
                        Err(e) => log::warn!("handle error: {e:?}"),
                    },
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        log::warn!("receive lagged, dropped {n} messages");
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        log::info!("receive loop closed");
                        break;
                    }
                }
            }
        });
    }

    async fn send_text(
        &self,
        c: &ImClient,
        chat: &ChatRef,
        my_handle: &str,
        text: String,
        guid: String,
    ) -> Result<IncomingMessage> {
        let imclient = client(c).clone();
        let conversation = conversation_from(chat);
        let normal = NormalMessage::new(text.clone(), MessageType::IMessage);
        let mut inst = MessageInst::new(conversation, my_handle, Message::Message(normal));
        inst.id = guid.clone();
        let date = now_ms();
        let inst = Mutex::new(inst);
        crate::retry::retry(3, std::time::Duration::from_millis(500), || async {
            let mut guard = inst.lock().await;
            imclient
                .send(&mut guard)
                .await
                .map_err(|e| anyhow::anyhow!("send failed: {e:?}"))
        })
        .await?;
        Ok(IncomingMessage {
            guid,
            chat: chat.clone(),
            sender: Some(my_handle.to_string()),
            is_from_me: true,
            text: Some(text),
            service: Some("iMessage".into()),
            date,
            ..Default::default()
        })
    }

    #[allow(clippy::too_many_arguments)]
    async fn send_reaction(
        &self,
        c: &ImClient,
        chat: &ChatRef,
        my_handle: &str,
        target_guid: &str,
        target_part: Option<u64>,
        target_text: &str,
        reaction: &ReactMessageType,
    ) -> Result<()> {
        let imclient = client(c).clone();
        let mut inst = build_react_message_inst(
            chat,
            my_handle,
            target_guid,
            target_part,
            target_text,
            reaction,
        );
        imclient
            .send(&mut inst)
            .await
            .map_err(|e| anyhow::anyhow!("send reaction failed: {e:?}"))?;
        Ok(())
    }

    async fn send_attachment(
        &self,
        c: &ImClient,
        connection: &Connection,
        chat: &ChatRef,
        my_handle: &str,
        path: String,
        mime: String,
        name: String,
        text: Option<String>,
        guid: String,
    ) -> Result<IncomingMessage> {
        use std::io::Seek;

        let imclient = client(c).clone();
        let aps = conn(connection).conn.clone();
        let conversation = conversation_from(chat);
        let uti = mime_to_uti(&mime);

        // Upload to MMCS (the file is read twice: once to prepare, once to send).
        let mut file = std::fs::File::open(&path)
            .map_err(|e| anyhow::anyhow!("open {path}: {e}"))?;
        let total = file.metadata().map(|m| m.len() as i64).ok();
        let prepared = MMCSFile::prepare_put(&mut file)
            .await
            .map_err(|e| anyhow::anyhow!("prepare attachment: {e:?}"))?;
        file.rewind().map_err(|e| anyhow::anyhow!("rewind: {e}"))?;
        let attachment =
            Attachment::new_mmcs(aps.inner(), &prepared, file, &mime, &uti, &name, |_, _| {})
                .await
                .map_err(|e| anyhow::anyhow!("upload attachment: {e:?}"))?;

        let mut normal = NormalMessage::new(String::new(), MessageType::IMessage);
        normal.parts = build_attachment_message_parts(text.as_deref(), attachment);
        let mut inst = MessageInst::new(conversation, my_handle, Message::Message(normal));
        inst.id = guid.clone();
        let date = now_ms();
        imclient
            .send(&mut inst)
            .await
            .map_err(|e| anyhow::anyhow!("send failed: {e:?}"))?;

        // Cache a local copy so the UI can render the sent image right away.
        let local_path = cache_copy(&path, &guid, 0, &name).map(|p| p.to_string_lossy().into_owned());

        Ok(IncomingMessage {
            guid,
            chat: chat.clone(),
            sender: Some(my_handle.to_string()),
            is_from_me: true,
            text,
            service: Some("iMessage".into()),
            date,
            attachments: vec![AttachmentRecord {
                mime: Some(mime),
                name: Some(name),
                total_bytes: total,
                local_path,
                part_index: Some(0),
                ..Default::default()
            }],
            ..Default::default()
        })
    }

    fn send_receipt(
        &self,
        c: &ImClient,
        chat: &ChatRef,
        my_handle: &str,
        read: bool,
        target_guid: String,
    ) {
        let imclient = client(c).clone();
        let conversation = conversation_from(chat);
        let my_handle = my_handle.to_string();
        crate::runtime::runtime().spawn(async move {
            let msg = if read { Message::Read } else { Message::Delivered };
            let mut inst = MessageInst::new(conversation, &my_handle, msg);
            inst.id = target_guid;
            let kind = if read { "read" } else { "delivered" };
            match imclient.send(&mut inst).await {
                Ok(_) => log::info!("→ sent {kind} receipt for {}", inst.id),
                Err(e) => log::warn!("{kind} receipt error: {e:?}"),
            }
        });
    }

    fn send_typing(&self, c: &ImClient, chat: &ChatRef, my_handle: &str, typing: bool) {
        let imclient = client(c).clone();
        let conversation = conversation_from(chat);
        let my_handle = my_handle.to_string();
        crate::runtime::runtime().spawn(async move {
            let mut inst =
                MessageInst::new(conversation, &my_handle, Message::Typing(typing, None));
            match imclient.send(&mut inst).await {
                Ok(_) => log::debug!("→ sent typing={typing}"),
                Err(e) => log::warn!("typing send error: {e:?}"),
            }
        });
    }

    fn sign_out(&self) {
        api::clear_session(&self.state_path);
    }
}

/// Spike-only: dump an inbound message's salient fields. `Message` doesn't
/// derive `Debug`, so we hand-format the variants we care about.
fn log_inst(inst: &MessageInst) {
    let (participants, name) = match &inst.conversation {
        Some(c) => (
            c.participants.join(", "),
            c.cv_name.clone().unwrap_or_default(),
        ),
        None => (String::new(), String::new()),
    };
    let body = match &inst.message {
        Message::Message(n) => {
            let mut s = format!("text={:?}", n.parts.raw_text());
            if let Some(r) = &n.reply_guid {
                s += &format!(" reply_to={r}");
            }
            if let Some(e) = &n.effect {
                s += &format!(" effect={e}");
            }
            if let Some(sub) = &n.subject {
                s += &format!(" subject={sub:?}");
            }
            s
        }
        other => format!("[{}]", variant_name(other)),
    };
    let name = if name.is_empty() {
        String::new()
    } else {
        format!(" name={name:?}")
    };
    log::info!(
        "RECV id={} ts={} sender={:?} chat=[{participants}]{name} {body}",
        inst.id,
        inst.sent_timestamp,
        inst.sender,
    );
}

fn variant_name(m: &Message) -> &'static str {
    match m {
        Message::Message(_) => "Message",
        Message::RenameMessage(_) => "Rename",
        Message::ChangeParticipants(_) => "ChangeParticipants",
        Message::React(_) => "React",
        Message::Delivered => "Delivered",
        Message::Read => "Read",
        Message::Typing(..) => "Typing",
        Message::Unsend(_) => "Unsend",
        Message::Edit(_) => "Edit",
        Message::IconChange(_) => "IconChange",
        Message::Error(_) => "Error",
        _ => "Other",
    }
}

/// Map a decrypted `MessageInst` into a store [`Ingest`]. `my_handles` is our
/// own address set (from `get_handles`), used to compute `is_from_me`. This is
/// the bridge receive loop will run before `Store::apply`.
pub fn ingest_from(inst: &MessageInst, my_handles: &[String]) -> Ingest {
    let guid = inst.id.clone();
    let date = inst.sent_timestamp as i64;
    let sender = inst.sender.clone();
    let is_from_me = inst
        .sender
        .as_deref()
        .map(|s| my_handles.iter().any(|h| h.eq_ignore_ascii_case(s)))
        .unwrap_or(false);

    // Receipts and tapbacks carry no conversation; content messages do.
    let chat = |service: Option<String>| -> ChatRef {
        match &inst.conversation {
            Some(c) => ChatRef {
                participants: c.participants.clone(),
                display_name: c.cv_name.clone(),
                service,
            },
            None => ChatRef::default(),
        }
    };

    match &inst.message {
        Message::Message(n) => {
            let service = Some(service_str(&n.service));
            Ingest::Message(IncomingMessage {
                guid,
                chat: chat(service.clone()),
                sender,
                is_from_me,
                text: Some(n.parts.raw_text()),
                subject: n.subject.clone(),
                service,
                date,
                effect: n.effect.clone(),
                reply_to_guid: n.reply_guid.clone(),
                reply_part: n.reply_part.clone(),
                item_type: 0,
                attachments: Vec::new(),
            })
        }
        Message::React(r) => match tapback_type(&r.reaction) {
            Some(associated_type) => Ingest::Tapback(Tapback {
                guid,
                chat: chat(None),
                sender,
                is_from_me,
                date,
                associated_guid: r.to_uuid.clone(),
                associated_part: r.to_part.map(|p| p.to_string()),
                associated_type,
            }),
            None => Ingest::Ignored("react-nonstandard"),
        },
        Message::Read => Ingest::Receipt(Receipt::Read { guid, date }),
        Message::Delivered => Ingest::Receipt(Receipt::Delivered { guid, date }),
        other => Ingest::Ignored(variant_name(other)),
    }
}

/// Where downloaded/sent attachment files live (mirrors `glib::user_data_dir`).
fn attachments_dir() -> std::path::PathBuf {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("HOME")
                .map(std::path::PathBuf::from)
                .unwrap_or_default()
                .join(".local/share")
        });
    base.join("bubbles").join("attachments")
}

fn ext_for(mime: &str, name: &str) -> String {
    if let Some(dot) = name.rfind('.') {
        if dot + 1 < name.len() {
            return name[dot..].to_string();
        }
    }
    match mime {
        "image/jpeg" => ".jpg",
        "image/png" => ".png",
        "image/gif" => ".gif",
        "image/heic" | "image/heif" => ".heic",
        "image/webp" => ".webp",
        "video/mp4" | "video/quicktime" => ".mp4",
        "application/pdf" => ".pdf",
        _ => ".bin",
    }
    .to_string()
}

fn mime_to_uti(mime: &str) -> String {
    match mime {
        "image/jpeg" => "public.jpeg",
        "image/png" => "public.png",
        "image/gif" => "com.compuserve.gif",
        "image/heic" | "image/heif" => "public.heic",
        "image/webp" => "org.webmproject.webp",
        "video/mp4" => "public.mpeg-4",
        "video/quicktime" => "com.apple.quicktime-movie",
        "application/pdf" => "com.adobe.pdf",
        _ => "public.data",
    }
    .to_string()
}

/// Copy an outbound file into the attachment cache so the UI can render it from
/// a stable path immediately after sending.
fn cache_copy(src: &str, guid: &str, part: i64, name: &str) -> Option<std::path::PathBuf> {
    let dir = attachments_dir();
    std::fs::create_dir_all(&dir).ok()?;
    let dest = dir.join(format!("{guid}_{part}{}", ext_for("", name)));
    std::fs::copy(src, &dest).ok()?;
    Some(dest)
}

/// Download every attachment on an inbound message into the cache, returning the
/// records to persist. Failures are logged and skipped, not fatal.
async fn download_inbound(
    inst: &MessageInst,
    conn: &APSConnection,
    guid: &str,
) -> Vec<AttachmentRecord> {
    let Message::Message(n) = &inst.message else {
        return Vec::new();
    };
    if !n.parts.has_attachments() {
        return Vec::new();
    }
    let dir = attachments_dir();
    if let Err(e) = std::fs::create_dir_all(&dir) {
        log::warn!("attachment dir: {e}");
        return Vec::new();
    }

    let mut out = Vec::new();
    for (i, p) in n.parts.0.iter().enumerate() {
        let MessagePart::Attachment(att) = &p.part else {
            continue;
        };
        let part_index = p.idx.unwrap_or(i) as i64;
        let path = dir.join(format!("{guid}_{part_index}{}", ext_for(&att.mime, &att.name)));
        let att_owned = att.clone();
        let conn_owned = conn.clone();
        match crate::attachment_cache::download_to_path(&path, move |file| {
            Box::pin(async move {
                att_owned.get_attachment(&conn_owned, file, |_, _| {}).await
            })
        }).await {
            Ok(()) => {
                log::info!("↓ saved attachment {} ({})", att.name, att.mime);
                out.push(AttachmentRecord {
                    guid: None,
                    mime: Some(att.mime.clone()),
                    name: Some(att.name.clone()),
                    total_bytes: Some(att.get_size() as i64),
                    local_path: Some(path.to_string_lossy().into_owned()),
                    part_index: Some(part_index),
                    is_sticker: false,
                });
            }
            Err(crate::attachment_cache::DownloadError::Download(e)) => {
                log::warn!("download attachment {}: {e:?}", att.name);
            }
            Err(crate::attachment_cache::DownloadError::Io(e)) => {
                log::warn!("io writing attachment {}: {e}", att.name);
            }
        }
    }
    out
}

fn service_str(t: &MessageType) -> String {
    match t {
        MessageType::IMessage => "iMessage".into(),
        MessageType::SMS { .. } => "SMS".into(),
    }
}

/// Apple tapback code: 2000-2005 add, 3000-3005 remove. `None` for emoji /
/// sticker / extension reactions we don't model yet (logged as Ignored).
fn tapback_type(t: &ReactMessageType) -> Option<i64> {
    let ReactMessageType::React { reaction, enable } = t else {
        return None;
    };
    let idx: i64 = match reaction {
        Reaction::Heart => 0,
        Reaction::Like => 1,
        Reaction::Dislike => 2,
        Reaction::Laugh => 3,
        Reaction::Emphasize => 4,
        Reaction::Question => 5,
        _ => return None,
    };
    Some(if *enable { 2000 + idx } else { 3000 + idx })
}

// --- link preview extraction ---

/// Pick the primary thumbnail blob out of a `LinkMeta`. The image is *not* a
/// normal MMCS attachment on the message — rustpush decodes the balloon body
/// and keeps its inline attachments here, indexed by the
/// `RichLinkImageAttachmentSubstitute` that the `LPLinkMetadata` carries.
/// `None` if the sender didn't include one.
fn preview_image_bytes(lm: &LinkMeta) -> Option<&[u8]> {
    let idx = lm.data.image.as_ref()?.rich_link_image_attachment_substitute_index as usize;
    lm.attachments.get(idx).map(|v| v.as_slice())
}

/// Pick a file extension for the thumbnail blob. Prefer the substitute's
/// `mime_type` (set by most modern iOS/macOS senders) and fall back to sniffing
/// the magic bytes. The renderer only cares that the file is the kind of image
/// the loader can decode; an unrecognised blob falls back to the neutral icon.
fn preview_image_ext(bytes: &[u8], mime: Option<&str>) -> &'static str {
    if let Some(m) = mime {
        let m = m.split(';').next().unwrap_or("").trim();
        match m {
            "image/png" => return "png",
            "image/jpeg" | "image/jpg" => return "jpg",
            "image/gif" => return "gif",
            "image/webp" => return "webp",
            "image/heic" | "image/heif" => return "heic",
            _ => {}
        }
    }
    if bytes.len() >= 8 && bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        "png"
    } else if bytes.len() >= 3 && bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        "jpg"
    } else if bytes.len() >= 6 && (bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a")) {
        "gif"
    } else if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        "webp"
    } else if bytes.len() >= 12 && &bytes[4..8] == b"ftyp" {
        // ISO base media: HEIC/HEIF/MP4 all share this. Default to .heic for
        // the common Apple case; the renderer can ignore what it can't decode.
        "heic"
    } else {
        "bin"
    }
}

/// Persist the thumbnail bytes to the cache and return the path. Errors are
/// logged and treated as "no image": a card with no thumbnail still renders
/// (using the neutral-icon fallback), so a flaky disk must not drop the link.
fn write_preview_image(guid: &str, part_idx: i64, bytes: &[u8], mime: Option<&str>) -> Option<String> {
    let dir = crate::store::preview_image_dir();
    if let Err(e) = std::fs::create_dir_all(&dir) {
        log::warn!("preview dir {dir:?}: {e}");
        return None;
    }
    let ext = preview_image_ext(bytes, mime);
    let path = dir.join(format!("{guid}_{part_idx}.{ext}"));
    match std::fs::write(&path, bytes) {
        Ok(()) => Some(path.to_string_lossy().into_owned()),
        Err(e) => {
            log::warn!("write preview {path:?}: {e}");
            None
        }
    }
}

/// URL the receiver should display / open. The `original_url` (what the sender
/// actually typed) wins when present and not blank — clicking the card opens
/// the *intended* link, not whatever redirect chain the sender's previewer
/// followed. We fall back to `url` (the post-redirect canonical) when the
/// original is missing, and to `None` when both are absent (degenerate).
fn preview_url(data: &LPLinkMetadata) -> Option<String> {
    if let Some(s) = original_url(data) {
        if !s.is_empty() {
            return Some(s);
        }
    }
    canonical_url(data).filter(|s| !s.is_empty())
}

fn canonical_url(data: &LPLinkMetadata) -> Option<String> {
    data.url.as_ref().map(|u| u.clone().into())
}

fn original_url(data: &LPLinkMetadata) -> Option<String> {
    data.original_url.as_ref().map(|u| u.clone().into())
}

/// Pull the (rare) `image_metadata` size string into our i64 width/height
/// fields. The size is "{w}x{h}" or "{w}×{h}" — best-effort, no fallback to
/// decoding the image (we don't want a synchronous decode on the receive path).
fn preview_dimensions(data: &LPLinkMetadata) -> (Option<i64>, Option<i64>) {
    let Some(s) = data.image_metadata.as_ref().map(|m| m.size.clone()) else {
        return (None, None);
    };
    parse_size_string(&s)
}

fn parse_size_string(s: &str) -> (Option<i64>, Option<i64>) {
    // Common separators: "1200x630", "1200×630", "1200 X 630".
    let cleaned: String = s
        .chars()
        .map(|c| if c == '\u{00D7}' || c == 'x' || c == 'X' { 'x' } else { c })
        .collect();
    let mut parts = cleaned.split('x');
    let w = parts.next().and_then(|p| p.trim().parse::<i64>().ok());
    let h = parts.next().and_then(|p| p.trim().parse::<i64>().ok());
    (w, h)
}

/// Build the `MessageLinkPreview` to persist for an inbound message, or `None`
/// if the message carries no `link_meta`. The thumbnail blob is written to the
/// cache here so the UI can read straight from disk later.
fn extract_link_preview(guid: &str, lm: &LinkMeta) -> Option<MessageLinkPreview> {
    let data = &lm.data;
    let url = preview_url(data);
    let original_url = original_url(data);
    let title = data.title.clone();
    let summary = data.summary.clone();
    let is_placeholder = data.is_incomplete.unwrap_or(false);
    let (image_width, image_height) = preview_dimensions(data);
    // Use the substitute's mime when the sender gave us one; fall through to
    // magic-byte sniffing for older senders / stripped payloads.
    let mime = data
        .image
        .as_ref()
        .map(|s| s.mime_type.as_str())
        .filter(|m| !m.is_empty());
    let image_path = preview_image_bytes(lm)
        .and_then(|bytes| write_preview_image(guid, 0, bytes, mime));
    Some(MessageLinkPreview {
        message_guid: guid.to_string(),
        part_idx: 0,
        url,
        original_url,
        title,
        summary,
        image_path,
        image_width,
        image_height,
        is_placeholder,
    })
}

/// Default: acknowledge inbound messages with Delivered receipts.
const SEND_DELIVERED_RECEIPTS: bool = true;

fn conversation_from(chat: &ChatRef) -> ConversationData {
    ConversationData {
        participants: chat.participants.clone(),
        cv_name: chat.display_name.clone(),
        sender_guid: None,
        after_guid: None,
    }
}

fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn is_from_me(inst: &MessageInst, handles: &[String]) -> bool {
    inst.sender
        .as_deref()
        .map(|s| handles.iter().any(|h| h.eq_ignore_ascii_case(s)))
        .unwrap_or(false)
}

fn is_incoming_content(inst: &MessageInst, handles: &[String]) -> bool {
    matches!(inst.message, Message::Message(_)) && !is_from_me(inst, handles)
}

/// Build a wire-level `ReactMessage` from the caller's parameters.
fn build_react_message(
    target_guid: &str,
    target_part: Option<u64>,
    to_text: &str,
    reaction: &ReactMessageType,
) -> ReactMessage {
    ReactMessage {
        to_uuid: target_guid.to_string(),
        to_part: target_part.or(Some(0)),
        reaction: reaction.clone(),
        to_text: to_text.to_string(),
        embedded_profile: None,
    }
}

/// Build a `MessageParts` wire payload for an iMessage that carries an
/// attachment plus an optional text caption.
///
/// When `text` is `Some(s)` the payload contains two parts in this order:
/// `MessagePart::Text` (caption) then `MessagePart::Attachment`.
/// When `text` is `None` the payload contains a single `MessagePart::Attachment`.
fn build_attachment_message_parts(text: Option<&str>, attachment: Attachment) -> MessageParts {
    match text {
        Some(s) => MessageParts(vec![
            IndexedMessagePart {
                part: MessagePart::Text(s.to_string(), Default::default()),
                idx: None,
                ext: None,
            },
            IndexedMessagePart {
                part: MessagePart::Attachment(attachment),
                idx: None,
                ext: None,
            },
        ]),
        None => MessageParts(vec![IndexedMessagePart {
            part: MessagePart::Attachment(attachment),
            idx: None,
            ext: None,
        }]),
    }
}

/// Generate a fresh GUID (uppercased UUID v4) for message identification.
fn new_guid() -> String {
    glib::uuid_string_random().to_string().to_uppercase()
}

/// Build a [`MessageInst`] ready to send as a reaction (tapback) to a target
/// message. Sets `inst.id` to a fresh GUID so the receiver can correlate it.
fn build_react_message_inst(
    chat: &ChatRef,
    my_handle: &str,
    target_guid: &str,
    target_part: Option<u64>,
    target_text: &str,
    reaction: &ReactMessageType,
) -> MessageInst {
    let conversation = conversation_from(chat);
    let react = build_react_message(target_guid, target_part, target_text, reaction);
    let mut inst = MessageInst::new(conversation, my_handle, Message::React(react));
    inst.id = new_guid();
    inst
}

/// Send a Delivered (`read=false`) or Read (`read=true`) receipt for `inst`,
/// addressed from whichever of our `handles` is in the conversation.
async fn send_receipt_for(
    imclient: &Arc<IMClient>,
    inst: &MessageInst,
    handles: &[String],
    read: bool,
) {
    let Some(conversation) = inst.conversation.clone() else {
        return;
    };
    let my_handle = conversation
        .participants
        .iter()
        .find(|p| handles.iter().any(|h| h.eq_ignore_ascii_case(p)))
        .cloned()
        .or_else(|| handles.first().cloned());
    let Some(my_handle) = my_handle else {
        return;
    };
    let msg = if read { Message::Read } else { Message::Delivered };
    let mut receipt = MessageInst::new(conversation, &my_handle, msg);
    receipt.id = inst.id.clone();
    let kind = if read { "read" } else { "delivered" };
    match imclient.send(&mut receipt).await {
        Ok(_) => log::info!("→ sent {kind} receipt for {}", receipt.id),
        Err(e) => log::warn!("{kind} receipt error: {e:?}"),
    }
}

#[cfg(test)]
mod tests {
    //! Tests for the `Backend` impl on `RustpushBackend` and its helper
    //! functions. The whole module is gated by `--features rustpush`, so this
    //! `mod tests` only compiles with the feature — matches the project's
    //! existing per-module cfg convention.
    use super::*;
    use rustpush::AttachmentType;

    /// Pin: the pure helper that builds the wire-level `ReactMessage` for
    /// `send_reaction`:
    ///
    /// * carries the caller's `(target_guid, reaction)` straight through to
    ///   `(to_uuid, reaction)` and leaves `embedded_profile` unset;
    /// * **defaults `to_part` to `Some(0)`** when the caller passes `None`
    ///   (matches Android OpenBubbles' `toPart: repPart ?? 0` so the iPhone
    ///   can resolve the `p:N/` part prefix on the target message);
    /// * passes through non-`None` `to_part` values unchanged (the default
    ///   must not override an explicit part index);
    /// * **threads `to_text` through** to the wire `ams` field (the iPhone
    ///   uses `ams` to render the reaction chip in the chat list).
    #[test]
    fn build_react_message_field_mapping() {
        // Case 1: `target_part = None` -> defaults to `Some(0)`; `to_text = "Hello world"` flows through.
        // Heart reaction, `enable = true`. This is the bug-repro case: the
        // pre-fix code passed `target_part = None` straight through, leaving
        // the `amk` field as a bare GUID with no `p:0/` prefix, so the iPhone
        // couldn't attach the reaction chip to the target message.
        let r1 = build_react_message(
            "target-guid-1",
            None,
            "Hello world",
            &ReactMessageType::React {
                reaction: Reaction::Heart,
                enable: true,
            },
        );
        assert_eq!(r1.to_uuid, "target-guid-1", "to_uuid should be the target_guid");
        assert_eq!(
            r1.to_part,
            Some(0),
            "to_part: None must default to Some(0) so the iPhone sees the p:0/ prefix"
        );
        assert_eq!(
            r1.to_text, "Hello world",
            "to_text should flow through to the wire field (ams)"
        );
        let (reaction, enable) = match r1.reaction.clone() {
            ReactMessageType::React { reaction, enable } => (reaction, enable),
            _ => panic!("expected ReactMessageType::React variant"),
        };
        assert!(
            matches!(reaction, Reaction::Heart),
            "reaction should be Heart"
        );
        assert!(enable, "enable should be true");
        assert!(
            r1.embedded_profile.is_none(),
            "embedded_profile should be None"
        );

        // Case 2: explicit `target_part = Some(0)` stays `Some(0)`; `to_text = ""` stays empty.
        // Like reaction, `enable = false`. Pins that the defaulting doesn't
        // re-default a part the caller already set, and that the empty
        // "last-resort" `to_text` value is preserved (the iPhone still has a
        // valid `ams` field, just an empty one).
        let r2 = build_react_message(
            "target-guid-2",
            Some(0),
            "",
            &ReactMessageType::React {
                reaction: Reaction::Like,
                enable: false,
            },
        );
        assert_eq!(r2.to_uuid, "target-guid-2", "to_uuid should be the target_guid");
        assert_eq!(
            r2.to_part,
            Some(0),
            "to_part: Some(0) should stay Some(0); the default must not override an explicit value"
        );
        assert!(
            r2.to_text.is_empty(),
            "to_text should stay empty when the caller passes \"\""
        );
        let (reaction, enable) = match r2.reaction.clone() {
            ReactMessageType::React { reaction, enable } => (reaction, enable),
            _ => panic!("expected ReactMessageType::React variant"),
        };
        assert!(
            matches!(reaction, Reaction::Like),
            "reaction should be Like"
        );
        assert!(!enable, "enable should be false");
        assert!(
            r2.embedded_profile.is_none(),
            "embedded_profile should be None"
        );

        // Case 3: explicit non-zero `target_part = Some(3)` is preserved.
        // Confirms the defaulting logic doesn't clobber a caller-supplied
        // part index (matters for multi-part messages where the part index
        // disambiguates which balloon the reaction targets).
        let r3 = build_react_message(
            "target-guid-3",
            Some(3),
            "Caption text",
            &ReactMessageType::React {
                reaction: Reaction::Heart,
                enable: true,
            },
        );
        assert_eq!(
            r3.to_part,
            Some(3),
            "to_part: Some(3) must be preserved, not overwritten with the default Some(0)"
        );
        assert_eq!(
            r3.to_text, "Caption text",
            "to_text should flow through to the wire field (ams)"
        );
    }

    /// Pin: the pure helper that builds the `MessageInst` for
    /// `send_reaction`:
    ///
    /// * sets `inst.id` to a non-empty, unique value (regression guard: the
    ///   pre-fix code path built the `MessageInst` inline and never assigned
    ///   `inst.id`, so reactions arrived at the receiver without a way to
    ///   attach to the target message);
    /// * wraps the caller's `(target_guid, target_part, reaction)` into a
    ///   `Message::React(...)` payload, **defaulting `target_part = None` to
    ///   `Some(0)`** in the wire payload;
    /// * **threads `target_text` through** to `inst.message`'s
    ///   `ReactMessage.to_text` field (the `ams` field the iPhone uses to
    ///   render the reaction chip);
    /// * carries `my_handle` through to `inst.sender`.
    ///
    /// Two calls must produce distinct ids — a correct implementation uses
    /// `new_guid()` (or an equivalent per-call GUID generator), not a
    /// hardcoded constant.
    #[test]
    fn build_react_message_inst_sets_id_and_payload() {
        let chat = ChatRef {
            participants: vec!["mailto:a@icloud.com".into()],
            display_name: None,
            service: Some("iMessage".into()),
        };
        let my_handle = "mailto:me@icloud.com";
        let reaction = ReactMessageType::React {
            reaction: Reaction::Heart,
            enable: true,
        };

        // Call with `target_part = None` (the UI's actual call site) — the
        // helper must default it to `Some(0)`, and `target_text` must reach
        // the wire `ams` field. Both are part of the fix.
        let inst = build_react_message_inst(
            &chat,
            my_handle,
            "target-guid",
            None,
            "Hello world",
            &reaction,
        );

        // 1. inst.id is non-empty — regression guard for the bug.
        assert!(
            !inst.id.is_empty(),
            "inst.id should be a freshly-generated GUID, not empty"
        );

        // 4. inst.sender carries the my_handle argument through.
        assert_eq!(
            inst.sender.as_deref(),
            Some(my_handle),
            "inst.sender should equal the my_handle argument"
        );

        // 3. inst.message is a Message::React carrying the caller's payload.
        // NOTE: `rustpush::Message` has only `Display`, no `Debug` — the
        // panic on a non-React variant must use `{other}`, not `{other:?}`.
        match &inst.message {
            Message::React(react) => {
                assert_eq!(react.to_uuid, "target-guid", "to_uuid should be target_guid");
                assert_eq!(
                    react.to_part,
                    Some(0),
                    "to_part: None must default to Some(0) on the wire payload"
                );
                assert_eq!(
                    react.to_text, "Hello world",
                    "to_text should flow through to inst.message's React payload (ams field)"
                );
                let (r, enable) = match react.reaction.clone() {
                    ReactMessageType::React { reaction, enable } => (reaction, enable),
                    _ => panic!("expected ReactMessageType::React variant"),
                };
                assert!(matches!(r, Reaction::Heart), "reaction payload should be Heart");
                assert!(enable, "enable should be true");
            }
            other => panic!("expected Message::React, got {other}"),
        }

        // 2. Two calls produce distinct ids — guards against hardcoding
        //    the fix to a constant GUID.
        let inst2 = build_react_message_inst(
            &chat,
            my_handle,
            "target-guid",
            None,
            "Hello world",
            &reaction,
        );
        assert_ne!(
            inst.id, inst2.id,
            "two calls to build_react_message_inst must produce distinct ids"
        );
    }

    /// Pin: the pure helper that builds the wire-level `MessageParts` for
    /// `send_attachment`:
    ///
    /// * **with `text = Some(...)`**: produces a two-part `MessageParts`,
    ///   `MessagePart::Text(...)` first then `MessagePart::Attachment(...)`,
    ///   both with `idx: None` and `ext: None`. The text content is preserved
    ///   as-is.
    /// * **with `text = None`**: produces a single-part `MessageParts`
    ///   containing only the `MessagePart::Attachment(...)` (photo-only case —
    ///   regression guard).
    ///
    /// This is the extracted seam that replaces the buggy inline construction
    /// in `send_attachment`, where `NormalMessage::new(text, ...)` correctly
    /// seeded the text part but the very next line overwrote `normal.parts`
    /// with a one-element `MessageParts` containing only the `Attachment`,
    /// dropping the caption from the wire payload.
    #[test]
    fn build_attachment_message_parts_cases() {
        fn fixture_attachment() -> Attachment {
            Attachment {
                a_type: AttachmentType::Inline(vec![]),
                part: 0,
                uti_type: "public.jpeg".into(),
                mime: "image/jpeg".into(),
                name: "photo.jpg".into(),
                iris: false,
            }
        }

        // -- Case 1: text = Some("hello") — two parts, text first. --
        let attach = fixture_attachment();
        let parts = build_attachment_message_parts(Some("hello"), attach.clone());

        assert_eq!(
            parts.0.len(),
            2,
            "with text: must produce exactly 2 IndexedMessagePart entries"
        );

        // First entry: MessagePart::Text with the caption, idx=None, ext=None.
        match &parts.0[0].part {
            MessagePart::Text(t, _) => assert_eq!(t, "hello", "text content must be preserved"),
            _ => panic!("parts[0] should be MessagePart::Text"),
        }
        assert_eq!(parts.0[0].idx, None, "parts[0].idx should be None");
        assert!(parts.0[0].ext.is_none(), "parts[0].ext should be None");

        // Second entry: MessagePart::Attachment with the same attachment,
        // idx=None, ext=None.  Pin the order: text first, then attachment.
        match &parts.0[1].part {
            MessagePart::Attachment(a) => {
                assert_eq!(a.part, attach.part, "attachment.part should match");
                assert_eq!(
                    a.uti_type, attach.uti_type,
                    "attachment.uti_type should match"
                );
                assert_eq!(a.mime, attach.mime, "attachment.mime should match");
                assert_eq!(a.name, attach.name, "attachment.name should match");
                assert_eq!(a.iris, attach.iris, "attachment.iris should match");
                match (&a.a_type, &attach.a_type) {
                    (AttachmentType::Inline(l), AttachmentType::Inline(r)) => {
                        assert_eq!(l, r, "attachment a_type (Inline) data should match")
                    }
                    _ => panic!("expected a_type to be Inline in both places"),
                }
            }
            _ => panic!("parts[1] should be MessagePart::Attachment"),
        }
        assert_eq!(parts.0[1].idx, None, "parts[1].idx should be None");
        assert!(parts.0[1].ext.is_none(), "parts[1].ext should be None");

        // -- Case 2: text = None — single attachment part only. --
        let attach2 = fixture_attachment();
        let parts2 = build_attachment_message_parts(None, attach2.clone());

        assert_eq!(
            parts2.0.len(),
            1,
            "without text: must produce exactly 1 IndexedMessagePart entry"
        );

        match &parts2.0[0].part {
            MessagePart::Attachment(a) => {
                assert_eq!(a.part, attach2.part, "attachment.part should match");
                assert_eq!(
                    a.uti_type, attach2.uti_type,
                    "attachment.uti_type should match"
                );
                assert_eq!(a.mime, attach2.mime, "attachment.mime should match");
                assert_eq!(a.name, attach2.name, "attachment.name should match");
                assert_eq!(a.iris, attach2.iris, "attachment.iris should match");
                match (&a.a_type, &attach2.a_type) {
                    (AttachmentType::Inline(l), AttachmentType::Inline(r)) => {
                        assert_eq!(l, r, "attachment a_type (Inline) data should match")
                    }
                    _ => panic!("expected a_type to be Inline in both places"),
                }
            }
            _ => panic!("single part should be MessagePart::Attachment"),
        }
        assert_eq!(parts2.0[0].idx, None, "single part idx should be None");
        assert!(parts2.0[0].ext.is_none(), "single part ext should be None");
    }
}
