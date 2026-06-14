//! Onboarding: the setup flow ([`flow`]) and its GTK view ([`view`]).

pub mod flow;
pub mod view;

use crate::protocol::{
    Account, Anisette, CircleSession, Config, Connection, Identity, IdsUser, ImClient, LoginState,
    VerifyBody,
};

/// State threaded across the onboarding pages. Lives in an `Rc<RefCell<_>>` on
/// the GTK main thread; handles are cloned out of it before each tokio call and
/// written back when the call resolves. Never borrowed across an `.await`.
#[derive(Default)]
pub struct SetupState {
    pub config: Option<Config>,
    pub connection: Option<Connection>,
    pub anisette: Option<Anisette>,
    pub identity: Option<Identity>,
    pub account: Option<Account>,
    pub apple_user: Option<IdsUser>,
    pub circle: Option<CircleSession>,
    pub verify_body: Option<VerifyBody>,
    pub login_state: LoginState,
    pub client: Option<ImClient>,
    pub handles: Vec<String>,
}
