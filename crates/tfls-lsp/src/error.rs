use thiserror::Error;

#[derive(Debug, Error)]
pub enum LspError {
    #[error("state error")]
    State(#[from] tfls_state::StateError),
}
