//! A no-op backend that returns canned values, so the UI compiles and the whole
//! onboarding flow can be clicked through before `rustpush` is linked.
//!
//! Replace with `RustpushBackend` (the lifted `api.rs`) when ready — the flow
//! and UI don't change, only the `Arc<dyn Backend>` handed to them in `main`.

use async_trait::async_trait;

use super::*;

/// Drives the flow down the trusted-device 2FA path and then to a logged-in
/// state with one phone handle.
pub struct StubBackend;

#[async_trait]
impl Backend for StubBackend {
    async fn config_from_relay(
        &self,
        _code: String,
        _host: String,
        _token: Option<String>,
    ) -> Result<Config> {
        Ok(Config::new(()))
    }

    async fn config_from_validation_data(
        &self,
        _data: Vec<u8>,
        _extra: HwExtra,
    ) -> Result<Config> {
        Ok(Config::new(()))
    }

    async fn config_from_encoded(&self, _encoded: Vec<u8>) -> Result<Config> {
        Ok(Config::new(()))
    }

    async fn device_info(&self, _config: &Config) -> Result<DeviceInfo> {
        Ok(DeviceInfo {
            name: "MacBookPro18,3".into(),
            serial: "C02XXXXXXXXX".into(),
            os_version: "14.5".into(),
        })
    }

    fn new_identity(&self) -> Result<Identity> {
        Ok(Identity::new(()))
    }

    async fn setup_push(&self, _config: &Config, _identity: &Identity) -> Result<Connection> {
        Ok(Connection::new(()))
    }

    async fn make_anisette(&self, _config: &Config, _conn: &Connection) -> Result<Anisette> {
        Ok(Anisette::new(()))
    }

    async fn try_auth(
        &self,
        _config: &Config,
        _conn: &Connection,
        _anisette: &Anisette,
        creds: Option<(String, String)>,
    ) -> Result<(Account, LoginState)> {
        let next = if creds.is_some() {
            // pretend the account has trusted devices -> ask for a code
            LoginState::NeedsDevice2Fa
        } else {
            LoginState::NeedsLogin
        };
        Ok((Account::new(()), next))
    }

    async fn try_icloud_login(
        &self,
        _config: &Config,
        _account: &Account,
    ) -> Result<Option<IdsUser>> {
        Ok(Some(IdsUser::new(())))
    }

    async fn send_2fa_to_devices(
        &self,
        _account: &Account,
        _conn: &Connection,
    ) -> Result<(CircleSession, LoginState)> {
        Ok((CircleSession::new(()), LoginState::Needs2FaVerification))
    }

    async fn verify_2fa(
        &self,
        _session: &CircleSession,
        _anisette: &Anisette,
        _config: &Config,
        _account: &Account,
        _code: String,
    ) -> Result<(LoginState, Option<IdsUser>)> {
        Ok((LoginState::LoggedIn, Some(IdsUser::new(()))))
    }

    async fn send_2fa_sms(&self, _account: &Account) -> Result<LoginState> {
        Ok(LoginState::NeedsSms2FaVerification(VerifyBody::new(())))
    }

    async fn verify_2fa_sms(
        &self,
        _account: &Account,
        _anisette: &Anisette,
        _config: &Config,
        _body: &VerifyBody,
        _code: String,
    ) -> Result<(LoginState, Option<IdsUser>)> {
        Ok((LoginState::LoggedIn, Some(IdsUser::new(()))))
    }

    async fn register_ids(
        &self,
        _config: &Config,
        _conn: &Connection,
        _identity: &Identity,
        _users: Vec<IdsUser>,
    ) -> Result<RegisterOutcome> {
        Ok(RegisterOutcome::Registered(vec![IdsUser::new(())]))
    }

    async fn make_imclient(
        &self,
        _conn: &Connection,
        _identity: &Identity,
        _users: Vec<IdsUser>,
    ) -> Result<ImClient> {
        Ok(ImClient::new(()))
    }

    async fn get_handles(&self, _client: &ImClient) -> Result<Vec<String>> {
        Ok(vec![
            "tel:+15555550123".into(),
            "mailto:you@example.com".into(),
        ])
    }

    async fn restore_session(&self) -> Result<Option<RestoredSession>> {
        // The stub never has a saved session; always run onboarding.
        Ok(None)
    }

    fn start_receiving(
        &self,
        _connection: &Connection,
        _client: &ImClient,
        _handles: Vec<String>,
        _store: crate::store::Store,
        _notify: async_channel::Sender<crate::protocol::RecvEvent>,
    ) {
        // No live connection in the stub; nothing to receive.
    }

    async fn send_text(
        &self,
        _client: &ImClient,
        chat: &crate::store::ChatRef,
        my_handle: &str,
        text: String,
        guid: String,
    ) -> Result<crate::store::IncomingMessage> {
        Ok(crate::store::IncomingMessage {
            guid,
            chat: chat.clone(),
            sender: Some(my_handle.to_string()),
            is_from_me: true,
            text: Some(text),
            date: 0,
            ..Default::default()
        })
    }
    async fn send_attachment(
        &self,
        _client: &ImClient,
        _connection: &Connection,
        chat: &crate::store::ChatRef,
        my_handle: &str,
        path: String,
        mime: String,
        name: String,
        guid: String,
    ) -> Result<crate::store::IncomingMessage> {
        Ok(crate::store::IncomingMessage {
            guid,
            chat: chat.clone(),
            sender: Some(my_handle.to_string()),
            is_from_me: true,
            date: 0,
            attachments: vec![crate::store::AttachmentRecord {
                mime: Some(mime),
                name: Some(name),
                local_path: Some(path),
                part_index: Some(0),
                ..Default::default()
            }],
            ..Default::default()
        })
    }

    fn send_receipt(
        &self,
        _client: &ImClient,
        _chat: &crate::store::ChatRef,
        _my_handle: &str,
        _read: bool,
        _target_guid: String,
    ) {
    }

    fn send_typing(
        &self,
        _client: &ImClient,
        _chat: &crate::store::ChatRef,
        _my_handle: &str,
        _typing: bool,
    ) {
    }

    fn sign_out(&self) {}
}

#[allow(dead_code)]
fn stub_nonce() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_chat_ref() -> crate::store::ChatRef {
        crate::store::ChatRef {
            participants: vec!["mailto:a@icloud.com".into(), "mailto:b@icloud.com".into()],
            display_name: None,
            service: Some("iMessage".into()),
        }
    }

    #[tokio::test]
    async fn send_attachment_without_caption() {
        let backend = StubBackend;
        let client = ImClient::new(());
        let connection = Connection::new(());
        let chat = sample_chat_ref();
        let my_handle = "tel:+15555550123";
        let path = "/tmp/test.jpg".to_string();
        let mime = "image/jpeg".to_string();
        let name = "photo.jpg".to_string();
        let guid = "guid-no-caption".to_string();

        let result = backend
            .send_attachment(
                &client,
                &connection,
                &chat,
                my_handle,
                path,
                mime,
                name,
                None,
                guid.clone(),
            )
            .await
            .expect("send_attachment should succeed");

        assert_eq!(result.text, None, "text should be None when no caption is provided");
        assert_eq!(result.attachments.len(), 1, "should have exactly one attachment");
    }

    #[tokio::test]
    async fn send_attachment_with_caption() {
        let backend = StubBackend;
        let client = ImClient::new(());
        let connection = Connection::new(());
        let chat = sample_chat_ref();
        let my_handle = "tel:+15555550123";
        let path = "/tmp/document.pdf".to_string();
        let mime = "application/pdf".to_string();
        let name = "report.pdf".to_string();
        let caption = "caption text".to_string();
        let guid = "guid-with-caption".to_string();

        let result = backend
            .send_attachment(
                &client,
                &connection,
                &chat,
                my_handle,
                path.clone(),
                mime.clone(),
                name.clone(),
                Some(caption.clone()),
                guid.clone(),
            )
            .await
            .expect("send_attachment should succeed");

        assert_eq!(
            result.text, Some(caption),
            "text should be Some(caption) when a caption is provided"
        );
        assert_eq!(result.attachments.len(), 1, "should have exactly one attachment");

        let attachment = &result.attachments[0];
        assert_eq!(
            attachment.mime.as_ref(),
            Some(&mime),
            "attachment mime should match the passed mime"
        );
        assert_eq!(
            attachment.name.as_ref(),
            Some(&name),
            "attachment name should match the passed name"
        );
        assert_eq!(
            attachment.local_path.as_ref(),
            Some(&path),
            "attachment local_path should match the passed path"
        );
    }
}
