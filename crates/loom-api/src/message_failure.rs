//! A completed send must expose its own message outcome to every transport.
#[derive(Debug)]
pub(crate) struct ActorMessageFailure {
    pub id: String,
    pub seq: i64,
    pub cause: String,
}
impl std::fmt::Display for ActorMessageFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "actor {} message {}: {}", self.id, self.seq, self.cause)
    }
}
impl std::error::Error for ActorMessageFailure {}
