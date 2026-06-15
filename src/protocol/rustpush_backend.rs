//! The real backend: implements [`Backend`] over `rustpush` + the vendored,
//! de-FRB'd `api.rs` (exposed as `crate::api`).
//!
//! NOT in the default build. Enable once you've vendored `api.rs` (+ its
//! siblings) and added the deps — see `PHASE_A.md`. Wire it with a cargo
//! feature, e.g. `#[cfg(feature = "rustpush")] pub mod rustpush_backend;` in
//! `protocol/mod.rs`, then swap `StubBackend` for `RustpushBackend::new(path)`
//! in `main`.
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
//!
//! This file is written against the confirmed signatures of api.rs @ a7fab47,
//! but is NOT compile-checked here (needs rustpush built). Spots I could not
//! verify are marked `// VERIFY`.

use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::{broadcast, Mutex};

use rustpush::{
    APSConnection, APSMessage, AppleAccount, ArcAnisetteClient, Attachment,
    CircleClientSession, ConversationData, DebugMutex, DefaultAnisetteProvider, IDSNGMIdentity,
    IDSUser, IMClient, IdmsAuthListener, IndexedMessagePart, LoginState as RpLoginState, MMCSFile,
    Message, MessageInst, MessagePart, MessageParts, MessageType, NormalMessage, Reaction,
    ReactMessageType, VerifyBody as RpVerifyBody,
};

use crate::store::{AttachmentRecord, ChatRef, IncomingMessage, Ingest, Receipt, Store, Tapback};

use crate::api;
use crate::protocol::*;

type Anis = ArcAnisetteClient<DefaultAnisetteProvider>;
// api.rs uses `rustpush::DebugMutex as Mutex`, so the account is wrapped in DebugMutex.
type AppleAcct = Arc<DebugMutex<AppleAccount<DefaultAnisetteProvider>>>;

/// Connection + the idms listener created alongside it (needed for 2FA verify).
struct ConnHandle {
    conn: APSConnection,
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
        // `state: None` = fresh connection. For session restore (Phase A2),
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
        let a = api::make_anisette(self.state_path.clone(), cfg(config), &conn(connection).conn).await;
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
            &conn(connection).conn,
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
        let (session, state, _sid) = api::send_2fa_to_devices(acct(account), &ch.conn).await?;
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
            &conn(connection).conn,
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
            &conn(connection).conn,
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
        let imclient = api::make_imclient(path.clone(), &conn, &users, &identity).await;
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
                                im.attachments = download_inbound(&inst, &conn, &im.guid).await;
                            }
                            if let Err(e) = store.apply(ingest).await {
                                log::warn!("store apply error: {e:#}");
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
        imclient
            .send(&mut inst)
            .await
            .map_err(|e| anyhow::anyhow!("send failed: {e:?}"))?;
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

    async fn send_attachment(
        &self,
        c: &ImClient,
        connection: &Connection,
        chat: &ChatRef,
        my_handle: &str,
        path: String,
        mime: String,
        name: String,
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
            Attachment::new_mmcs(&aps, &prepared, file, &mime, &uti, &name, |_, _| {})
                .await
                .map_err(|e| anyhow::anyhow!("upload attachment: {e:?}"))?;

        let mut normal = NormalMessage::new(String::new(), MessageType::IMessage);
        normal.parts = MessageParts(vec![IndexedMessagePart {
            part: MessagePart::Attachment(attachment),
            idx: None,
            ext: None,
        }]);
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
/// the bridge Phase C's receive loop will run before `Store::apply`.
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
    base.join("openbubbles-gtk").join("attachments")
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
    use std::io::Write;
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
        let mut file = match std::fs::File::create(&path) {
            Ok(f) => f,
            Err(e) => {
                log::warn!("create {}: {e}", path.display());
                continue;
            }
        };
        match att.get_attachment(conn, &mut file, |_, _| {}).await {
            Ok(()) => {
                let _ = file.flush();
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
            Err(e) => {
                log::warn!("download attachment {}: {e:?}", att.name);
                let _ = std::fs::remove_file(&path);
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

/// Phase D default: acknowledge inbound messages with Delivered receipts.
/// Becomes a user setting once a settings module exists.
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
