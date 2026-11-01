//! L2 `svc-sync` — 云同步预留（v1 空实现，PLAN.md §6.1/§5）。

#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    #[error("cloud sync not implemented in v1")]
    NotImplemented,
}
