//! The onboarding state machine, ported from `setup_view.dart`.
//!
//! These are free `async` functions over [`Backend`]: they take owned, cloned
//! handles and return owned results. Crucially they hold no `RefCell` borrow
//! across an `.await`, so the GTK side can clone what it needs out of
//! [`super::SetupState`], run the future on tokio, and write the result back
//! when it resolves (see [`crate::gtk_bridge::spawn`]).

use std::sync::Arc;

use anyhow::{bail, Context};

use crate::protocol::{
    Account, Anisette, Backend, CircleSession, Config, Connection, DeviceInfo, HwExtra, Identity,
    IdsUser, ImClient, LoginState, RegisterOutcome, RestoredSession, Result, SupportAlert,
    VerifyBody,
};

/// The push connection + identity + anisette produced by [`prepare_connection`].
pub struct Connected {
    pub identity: Identity,
    pub connection: Connection,
    pub anisette: Anisette,
}

/// Step 2: identity + push connection + anisette.
pub async fn prepare_connection(b: Arc<dyn Backend>, config: Config) -> Result<Connected> {
    let identity = b.new_identity()?;
    let connection = b.setup_push(&config, &identity).await?;
    let anisette = b.make_anisette(&config, &connection).await?;
    Ok(Connected {
        identity,
        connection,
        anisette,
    })
}

/// Everything produced by step 1+2 from a relay pairing code.
pub struct HardwareReady {
    pub config: Config,
    pub device: DeviceInfo,
    pub connected: Connected,
}

/// Step 1 (relay path) + step 2, in one shot for the hardware page.
pub async fn connect_relay(
    b: Arc<dyn Backend>,
    code: String,
    host: String,
    token: Option<String>,
) -> Result<HardwareReady> {
    let config = b.config_from_relay(code, host, token).await?;
    let device = b.device_info(&config).await?;
    let connected = prepare_connection(b.clone(), config.clone()).await?;
    Ok(HardwareReady {
        config,
        device,
        connected,
    })
}

/// Step 1 (local macOS hardware) + step 2. `data` is the raw 517-byte
/// validation data extracted once from a Mac; the backend then regenerates
/// fresh validation data itself via the bundled FairPlay certs, so the Mac is
/// only needed for this one bootstrap. No relay or reservation involved.
pub async fn connect_local(b: Arc<dyn Backend>, data: Vec<u8>) -> Result<HardwareReady> {
    let config = b.config_from_validation_data(data, HwExtra {}).await?;
    let device = b.device_info(&config).await?;
    let connected = prepare_connection(b.clone(), config.clone()).await?;
    Ok(HardwareReady {
        config,
        device,
        connected,
    })
}

/// Step 1 (cached encoded hardware blob) + step 2. `encoded` is the
/// OABS-stripped `bbhwinfo` payload produced by a prior local pairing.
pub async fn connect_encoded(b: Arc<dyn Backend>, encoded: Vec<u8>) -> Result<HardwareReady> {
    let config = b.config_from_encoded(encoded).await?;
    let device = b.device_info(&config).await?;
    let connected = prepare_connection(b.clone(), config.clone()).await?;
    Ok(HardwareReady {
        config,
        device,
        connected,
    })
}
pub struct LoginAdvance {
    pub state: LoginState,
    pub account: Option<Account>,
    pub apple_user: Option<IdsUser>,
    pub circle: Option<CircleSession>,
}

/// Step 3: `updateLoginState`. Drives `NeedsLogin -> {Needs*2Fa} -> verification`
/// without ever blocking the UI thread.
pub async fn advance_login(
    b: Arc<dyn Backend>,
    config: Config,
    connection: Connection,
    anisette: Anisette,
    account: Option<Account>,
    circle: Option<CircleSession>,
    creds: Option<(String, String)>,
    mut state: LoginState,
) -> Result<LoginAdvance> {
    let mut account = account;
    let mut apple_user = None;
    let mut circle = circle;

    if matches!(state, LoginState::NeedsLogin) {
        let (acct, next) = b.try_auth(&config, &connection, &anisette, creds).await?;
        apple_user = b.try_icloud_login(&config, &acct).await?;
        account = Some(acct);
        state = next;
    }

    if matches!(state, LoginState::NeedsDevice2Fa) {
        let acct = account.as_ref().context("no account for device 2FA")?;
        let (session, next) = b.send_2fa_to_devices(acct, &connection).await?;
        circle = Some(session);
        state = next;
    }

    if matches!(state, LoginState::NeedsSms2Fa) {
        let acct = account.as_ref().context("no account for SMS 2FA")?;
        state = b.send_2fa_sms(acct).await?;
    }

    Ok(LoginAdvance {
        state,
        account,
        apple_user,
        circle,
    })
}

/// Result of submitting a 2FA code.
pub struct CodeResult {
    pub state: LoginState,
    pub apple_user: Option<IdsUser>,
}

/// Step 3 (cont.): `submitCode`. Routes to device or SMS verification by state.
pub async fn submit_code(
    b: Arc<dyn Backend>,
    config: Config,
    anisette: Anisette,
    account: Account,
    circle: Option<CircleSession>,
    verify_body: Option<VerifyBody>,
    state: LoginState,
    code: String,
) -> Result<CodeResult> {
    match state {
        LoginState::Needs2FaVerification => {
            let session = circle.as_ref().context("missing circle session")?;
            let (state, apple_user) = b
                .verify_2fa(session, &anisette, &config, &account, code)
                .await?;
            Ok(CodeResult { state, apple_user })
        }
        LoginState::NeedsSms2FaVerification(_) => {
            let body = verify_body.as_ref().context("missing SMS verify body")?;
            let (state, apple_user) = b
                .verify_2fa_sms(&account, &anisette, &config, body, code)
                .await?;
            Ok(CodeResult { state, apple_user })
        }
        other => bail!("submit_code called in non-verification state: {other:?}"),
    }
}

/// Result of a successful registration.
pub struct Registered {
    pub client: ImClient,
    pub handles: Vec<String>,
}

/// Step 4: `doRegister`. Returns `Err(SupportAlert)` inside `Ok` when Apple
/// blocks registration (a normal, displayable outcome, not a transport error).
pub async fn register(
    b: Arc<dyn Backend>,
    config: Config,
    connection: Connection,
    identity: Identity,
    apple_user: Option<IdsUser>,
) -> Result<std::result::Result<Registered, SupportAlert>> {
    let mut users = Vec::new();
    if let Some(u) = apple_user {
        users.push(u);
    }
    if users.is_empty() {
        bail!("no users to register");
    }

    match b.register_ids(&config, &connection, &identity, users).await? {
        RegisterOutcome::Blocked(alert) => Ok(Err(alert)),
        RegisterOutcome::Registered(new_users) => {
            let client = b.make_imclient(&connection, &identity, new_users).await?;
            let handles = b.get_handles(&client).await?;
            Ok(Ok(Registered { client, handles }))
        }
    }
}

/// Phase A2: attempt to restore a registered session from disk before showing
/// any onboarding UI. `Ok(None)` means nothing is saved and the caller should
/// run the normal flow.
pub async fn restore(b: Arc<dyn Backend>) -> Result<Option<RestoredSession>> {
    b.restore_session().await
}
