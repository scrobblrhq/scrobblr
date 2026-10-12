//! Erases deleted accounts once their grace period is over, and their
//! uploaded files (best-effort: `worker uploads gc` finds what's left).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, bail};
use sqlx::PgPool;

use crate::heartbeat::Beat;
use db::queries::{accounts as accounts_db, users as users_db};
use shared::storage::{LocalStorage, Storage};

const POLL_INTERVAL: Duration = Duration::from_secs(60);

pub const USAGE: &str = "       worker accounts pending     list the accounts waiting to be erased
       worker accounts erase NAME  erase the account NAME now, deleted or not: its
                                  credentials are revoked and everything about it
                                  goes (for a request made outside the app)";

pub struct Eraser {
    db: PgPool,
    /// `None` without access to `UPLOAD_DIR`.
    storage: Option<Arc<Storage>>,
}

impl Eraser {
    pub fn from_env(db: PgPool) -> Self {
        let root =
            PathBuf::from(crate::non_empty_env("UPLOAD_DIR").unwrap_or_else(|| "uploads".into()));
        let storage = if root.is_dir() {
            match LocalStorage::open(root.clone()) {
                Ok(local) => Some(Arc::new(Storage::Local(local))),
                Err(e) => {
                    tracing::warn!(
                        "accounts: UPLOAD_DIR {} unusable, erased accounts' files are left to \
                         `worker uploads gc`: {e}",
                        root.display()
                    );
                    None
                }
            }
        } else {
            tracing::warn!(
                "accounts: UPLOAD_DIR {} is not a directory here, erased accounts' files are \
                 left to `worker uploads gc`",
                root.display()
            );
            None
        };
        Self { db, storage }
    }

    pub async fn run(self: Arc<Self>, beat: Beat) {
        let mut interval = tokio::time::interval(POLL_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            match self.erase_due().await {
                Ok(_) => beat.ok().await,
                Err(e) => {
                    tracing::error!("accounts: erase failed, retrying later: {e}");
                    beat.failed(e).await;
                }
            }
        }
    }

    async fn erase_due(&self) -> Result<u32, sqlx::Error> {
        let mut erased = 0;
        while let Some(user_id) = accounts_db::claim_due(&self.db).await? {
            self.erase(user_id).await?;
            erased += 1;
        }
        Ok(erased)
    }

    async fn erase(&self, user_id: i64) -> Result<(), sqlx::Error> {
        let erased = accounts_db::erase(&self.db, user_id).await?;
        let mut files_left = 0;
        for key in &erased.uploads {
            match &self.storage {
                Some(storage) => {
                    if let Err(e) = storage.delete(key).await {
                        tracing::warn!(%key, "accounts: could not delete an erased account's upload: {e}");
                        files_left += 1;
                    }
                }
                None => files_left += 1,
            }
        }
        tracing::info!(
            user_id,
            scrobbles = erased.scrobbles,
            chunks = erased.chunks,
            aggregate_rows = erased.aggregate_rows,
            aggregates_by_refresh = erased.aggregates_by_refresh,
            uploads = erased.uploads.len(),
            files_left,
            took_ms = erased.took_ms as u64,
            "accounts: erased"
        );
        Ok(())
    }
}

pub async fn run(db: &PgPool, args: &[String]) -> anyhow::Result<()> {
    match args {
        [cmd] if cmd == "pending" => {
            let pending = accounts_db::pending(db).await?;
            if pending.is_empty() {
                println!("no account is waiting to be erased");
            }
            for p in pending {
                let state = match p.erase_started_at {
                    Some(at) => format!("erase started {}", at.format("%Y-%m-%d %H:%M UTC")),
                    None => format!(
                        "erased after {}",
                        p.delete_after.format("%Y-%m-%d %H:%M UTC")
                    ),
                };
                println!("{:>10}  {:<30}  {state}", p.user_id, p.username);
            }
            Ok(())
        }
        [cmd, name] if cmd == "erase" => {
            let user = users_db::find_by_username(db, name)
                .await?
                .with_context(|| format!("no user named `{name}`"))?;
            let scheduled =
                accounts_db::schedule_deletion(db, user.id, chrono::TimeDelta::zero()).await?;
            accounts_db::mark_erasing(db, user.id).await?;
            forget_sessions(&scheduled.sessions).await;
            Eraser::from_env(db.clone()).erase(user.id).await?;
            println!("erased `{}` (#{})", user.username, user.id);
            Ok(())
        }
        _ => bail!("{USAGE}"),
    }
}

async fn forget_sessions(sessions: &[uuid::Uuid]) {
    if sessions.is_empty() {
        return;
    }
    let Some(redis) = crate::connect_redis().await else {
        eprintln!(
            "warning: no Redis: the account's {} session(s) stay in its cache until they expire",
            sessions.len()
        );
        return;
    };
    for id in sessions {
        if let Err(e) =
            fred::interfaces::KeysInterface::del::<i64, _>(&redis, format!("session:{id}")).await
        {
            eprintln!("warning: could not remove session {id} from the Redis cache: {e}");
        }
    }
}
