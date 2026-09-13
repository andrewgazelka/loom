//! A completed send must expose its own message outcome to every transport.
#[derive(Debug)]
pub(crate) struct ActorMessageFailure {
    pub id: String,
    pub seq: i64,
    pub cause: String,
    pub outcome: MessageOutcome,
}
#[derive(Debug, Clone, Copy)]
pub(crate) enum MessageOutcome { Failed, Pending }
impl ActorMessageFailure {
    pub fn code(&self) -> &'static str {
        match self.outcome {
            MessageOutcome::Failed => "actor_message_failed",
            MessageOutcome::Pending => "actor_message_pending",
        }
    }
}
impl std::fmt::Display for ActorMessageFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "actor {} seq {}: send {}: {}", self.id, self.seq, self.code(), self.cause)
    }
}
impl std::error::Error for ActorMessageFailure {}
