use std::{error::Error, fmt::Display};

pub mod nac;
mod jelly;
mod hooks;

#[derive(Debug)]
pub struct AbsintheError(pub(crate) i32);

impl AbsintheError {
    pub(crate) fn new(code: i32) -> Self { AbsintheError(code) }
}

impl Display for AbsintheError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "absinthe error {}", self.0)
    }
}

impl Error for AbsintheError { }

impl From<unicorn_engine::unicorn_const::uc_error> for AbsintheError {
    fn from(e: unicorn_engine::unicorn_const::uc_error) -> Self {
        log::error!("unicorn error: {e:?}");
        AbsintheError(-(e as i32) - 1000)
    }
}

impl From<goblin::error::Error> for AbsintheError {
    fn from(e: goblin::error::Error) -> Self {
        log::error!("goblin error: {e:?}");
        AbsintheError(-2000)
    }
}
