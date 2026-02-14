/// Strongly typed wrapper for conversation row IDs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, sqlx::Type)]
#[sqlx(transparent)]
pub struct ConversationId(pub i64);

/// Strongly typed wrapper for message row IDs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, sqlx::Type)]
#[sqlx(transparent)]
pub struct MessageId(pub i64);

impl std::fmt::Display for ConversationId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::fmt::Display for MessageId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conversation_id_display() {
        let id = ConversationId(42);
        assert_eq!(format!("{id}"), "42");
    }

    #[test]
    fn message_id_display() {
        let id = MessageId(7);
        assert_eq!(format!("{id}"), "7");
    }

    #[test]
    fn conversation_id_eq() {
        assert_eq!(ConversationId(1), ConversationId(1));
        assert_ne!(ConversationId(1), ConversationId(2));
    }

    #[test]
    fn message_id_copy() {
        let id = MessageId(5);
        let copied = id;
        assert_eq!(id, copied);
    }
}
