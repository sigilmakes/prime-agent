//! Envelope-free metadata appends.
use super::{AgentMessage, FileEntry, SessionManager};
impl SessionManager {
    pub(crate) fn append_metadata_intent(
        &mut self,
        mut entry: FileEntry,
    ) -> std::io::Result<String> {
        let (FileEntry::ThinkingLevelChange { base, .. }
        | FileEntry::ServiceTierChange { base, .. }
        | FileEntry::ModelChange { base, .. }
        | FileEntry::Custom { base, .. }
        | FileEntry::ChildUsageAttributed { base, .. }
        | FileEntry::Label { base, .. }
        | FileEntry::SessionInfo { base, .. }
        | FileEntry::SessionState { base, .. }
        | FileEntry::GitState { base, .. }
        | FileEntry::CustomMessage { base, .. }
        | FileEntry::Message {
            base,
            message: AgentMessage::BashExecution(_),
        }) = &mut entry
        else {
            return Err(std::io::Error::other(
                "metadata intent is not a metadata action",
            ));
        };
        if base.id.is_some() || base.parent_id.is_some() || base.timestamp.is_some() {
            return Err(std::io::Error::other(
                "metadata intent supplied canonical identity",
            ));
        }
        *base = self.next_base()?;
        self.append_entry(entry)
    }
}
