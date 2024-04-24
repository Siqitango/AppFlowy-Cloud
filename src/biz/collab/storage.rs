use crate::biz::casbin::{CollabAccessControlImpl, WorkspaceAccessControlImpl};
use crate::biz::collab::access_control::CollabStorageAccessControlImpl;
use crate::biz::collab::cache::CollabCache;
use crate::biz::snapshot::SnapshotControl;

use app_error::AppError;
use async_trait::async_trait;

use collab::core::collab_plugin::EncodedCollab;

use appflowy_collaborate::command::{RTCommand, RTCommandSender};
use database::collab::{AppResult, CollabStorage, CollabStorageAccessControl};
use database_entity::dto::{
  AFAccessLevel, AFSnapshotMeta, AFSnapshotMetas, CollabParams, InsertSnapshotParams, QueryCollab,
  QueryCollabParams, QueryCollabResult, SnapshotData,
};
use itertools::{Either, Itertools};

use crate::state::RedisConnectionManager;
use appflowy_collaborate::data_validation::CollabValidator;
use sqlx::Transaction;
use std::collections::HashMap;
use std::sync::Arc;

use crate::biz::collab::metrics::CollabMetrics;
use crate::biz::collab::queue::{StorageQueue, REDIS_PENDING_WRITE_QUEUE};
use crate::biz::collab::queue_redis_ops::WritePriority;
use std::time::Duration;
use tokio::sync::oneshot;
use tokio::time::timeout;
use tracing::{error, instrument, trace};
use validator::Validate;

pub type CollabAccessControlStorage = CollabStorageImpl<
  CollabStorageAccessControlImpl<CollabAccessControlImpl, WorkspaceAccessControlImpl>,
>;

/// A wrapper around the actual storage implementation that provides access control and caching.
#[derive(Clone)]
pub struct CollabStorageImpl<AC> {
  cache: CollabCache,
  /// access control for collab object. Including read/write
  access_control: AC,
  snapshot_control: SnapshotControl,
  rt_cmd_sender: RTCommandSender,
  queue: Arc<StorageQueue>,
}

impl<AC> CollabStorageImpl<AC>
where
  AC: CollabStorageAccessControl,
{
  pub fn new(
    cache: CollabCache,
    access_control: AC,
    snapshot_control: SnapshotControl,
    rt_cmd_sender: RTCommandSender,
    redis_conn_manager: RedisConnectionManager,
    metrics: Arc<CollabMetrics>,
  ) -> Self {
    let queue = Arc::new(StorageQueue::new_with_metrics(
      cache.clone(),
      redis_conn_manager,
      REDIS_PENDING_WRITE_QUEUE,
      Some(metrics),
    ));
    Self {
      cache,
      access_control,
      snapshot_control,
      rt_cmd_sender,
      queue,
    }
  }

  async fn check_write_workspace_permission(
    &self,
    workspace_id: &str,
    uid: &i64,
  ) -> Result<(), AppError> {
    // If the collab doesn't exist, check if the user has enough permissions to create collab.
    // If the user is the owner or member of the workspace, the user can create collab.
    let can_write_workspace = self
      .access_control
      .enforce_write_workspace(uid, workspace_id)
      .await?;

    if !can_write_workspace {
      return Err(AppError::NotEnoughPermissions {
        user: uid.to_string(),
        action: format!("write workspace:{}", workspace_id),
      });
    }
    Ok(())
  }

  async fn check_write_collab_permission(
    &self,
    workspace_id: &str,
    uid: &i64,
    object_id: &str,
  ) -> Result<(), AppError> {
    // If the collab already exists, check if the user has enough permissions to update collab
    let can_write = self
      .access_control
      .enforce_write_collab(workspace_id, uid, object_id)
      .await?;

    if !can_write {
      return Err(AppError::NotEnoughPermissions {
        user: uid.to_string(),
        action: format!("update collab:{}", object_id),
      });
    }
    Ok(())
  }

  async fn get_encode_collab_from_editing(&self, object_id: &str) -> Option<EncodedCollab> {
    let object_id = object_id.to_string();
    let (ret, rx) = oneshot::channel();
    let timeout_duration = Duration::from_secs(5);

    // Attempt to send the command to the realtime server
    if let Err(err) = self
      .rt_cmd_sender
      .send(RTCommand::GetEncodeCollab { object_id, ret })
      .await
    {
      error!(
        "Failed to send get encode collab command to realtime server: {}",
        err
      );
      return None;
    }

    // Await the response from the realtime server with a timeout
    match timeout(timeout_duration, rx).await {
      Ok(Ok(Some(encode_collab))) => Some(encode_collab),
      Ok(Ok(None)) => {
        trace!("No encode collab found in editing collab");
        None
      },
      Ok(Err(err)) => {
        error!("Failed to get encode collab from realtime server: {}", err);
        None
      },
      Err(_) => {
        error!("Timeout waiting for encode collab from realtime server");
        None
      },
    }
  }

  async fn queue_insert_collab(
    &self,
    workspace_id: &str,
    uid: &i64,
    params: CollabParams,
    priority: WritePriority,
  ) -> Result<(), AppError> {
    if let Err(err) = params.check_encode_collab().await {
      return Err(AppError::NoRequiredData(format!(
        "Invalid collab doc state detected for workspace_id: {}, uid: {}, object_id: {} collab_type:{}. Error details: {}",
        workspace_id, uid, params.object_id, params.collab_type, err
      )));
    }

    self
      .queue
      .push(workspace_id, uid, &params, priority)
      .await
      .map_err(AppError::from)
  }
}

#[async_trait]
impl<AC> CollabStorage for CollabStorageImpl<AC>
where
  AC: CollabStorageAccessControl,
{
  fn encode_collab_redis_query_state(&self) -> (u64, u64) {
    let state = self.cache.query_state();
    (state.total_attempts, state.success_attempts)
  }

  async fn insert_or_update_collab(
    &self,
    workspace_id: &str,
    uid: &i64,
    params: CollabParams,
    write_immediately: bool,
  ) -> AppResult<()> {
    params.validate()?;
    let is_exist = self.cache.is_exist(&params.object_id).await?;
    // If the collab already exists, check if the user has enough permissions to update collab
    // Otherwise, check if the user has enough permissions to create collab.
    if is_exist {
      self
        .check_write_collab_permission(workspace_id, uid, &params.object_id)
        .await?;
    } else {
      self
        .check_write_workspace_permission(workspace_id, uid)
        .await?;
      trace!(
        "Update policy for user:{} to create collab:{}",
        uid,
        params.object_id
      );
      self
        .access_control
        .update_policy(uid, &params.object_id, AFAccessLevel::FullAccess)
        .await?;
    }
    let priority = if write_immediately {
      WritePriority::High
    } else {
      WritePriority::Low
    };
    self
      .queue_insert_collab(workspace_id, uid, params, priority)
      .await?;
    Ok(())
  }

  async fn insert_new_collab(
    &self,
    workspace_id: &str,
    uid: &i64,
    params: CollabParams,
  ) -> AppResult<()> {
    params.validate()?;

    self
      .check_write_workspace_permission(workspace_id, uid)
      .await?;
    self
      .access_control
      .update_policy(uid, &params.object_id, AFAccessLevel::FullAccess)
      .await?;
    self
      .queue_insert_collab(workspace_id, uid, params, WritePriority::High)
      .await?;
    Ok(())
  }

  #[instrument(level = "trace", skip(self, params), oid = %params.oid, ty = %params.collab_type, err)]
  #[allow(clippy::blocks_in_conditions)]
  async fn insert_new_collab_with_transaction(
    &self,
    workspace_id: &str,
    uid: &i64,
    params: CollabParams,
    transaction: &mut Transaction<'_, sqlx::Postgres>,
  ) -> AppResult<()> {
    params.validate()?;
    self
      .check_write_workspace_permission(workspace_id, uid)
      .await?;
    self
      .access_control
      .update_policy(uid, &params.object_id, AFAccessLevel::FullAccess)
      .await?;
    self
      .cache
      .insert_encode_collab_data(workspace_id, uid, &params, transaction)
      .await?;
    Ok(())
  }

  #[instrument(level = "trace", skip_all, fields(oid = %params.object_id, is_collab_init = %is_collab_init))]
  async fn get_encode_collab(
    &self,
    uid: &i64,
    params: QueryCollabParams,
    is_collab_init: bool,
  ) -> AppResult<EncodedCollab> {
    params.validate()?;

    // Check if the user has enough permissions to access the collab
    let can_read = self
      .access_control
      .enforce_read_collab(&params.workspace_id, uid, &params.object_id)
      .await?;

    if !can_read {
      return Err(AppError::NotEnoughPermissions {
        user: uid.to_string(),
        action: format!("read collab:{}", params.object_id),
      });
    }

    // Early return if editing collab is initialized, as it indicates no need to query further.
    if !is_collab_init {
      trace!("Get encode collab {} from editing collab", params.object_id);
      // Attempt to retrieve encoded collab from the editing collab
      if let Some(value) = self.get_encode_collab_from_editing(&params.object_id).await {
        return Ok(value);
      }
    }

    let encode_collab = self.cache.get_encode_collab(uid, params.inner).await?;
    Ok(encode_collab)
  }

  async fn batch_get_collab(
    &self,
    _uid: &i64,
    queries: Vec<QueryCollab>,
  ) -> HashMap<String, QueryCollabResult> {
    // Partition queries based on validation into valid queries and errors (with associated error messages).
    let (valid_queries, mut results): (Vec<_>, HashMap<_, _>) =
      queries
        .into_iter()
        .partition_map(|params| match params.validate() {
          Ok(_) => Either::Left(params),
          Err(err) => Either::Right((
            params.object_id,
            QueryCollabResult::Failed {
              error: err.to_string(),
            },
          )),
        });

    results.extend(self.cache.batch_get_encode_collab(valid_queries).await);
    results
  }

  async fn delete_collab(&self, workspace_id: &str, uid: &i64, object_id: &str) -> AppResult<()> {
    if !self
      .access_control
      .enforce_delete(workspace_id, uid, object_id)
      .await?
    {
      return Err(AppError::NotEnoughPermissions {
        user: uid.to_string(),
        action: format!("delete collab:{}", object_id),
      });
    }
    self.cache.delete_collab(object_id).await?;
    Ok(())
  }

  async fn should_create_snapshot(&self, oid: &str) -> Result<bool, AppError> {
    self.snapshot_control.should_create_snapshot(oid).await
  }

  async fn create_snapshot(&self, params: InsertSnapshotParams) -> AppResult<AFSnapshotMeta> {
    self.snapshot_control.create_snapshot(params).await
  }

  async fn queue_snapshot(&self, params: InsertSnapshotParams) -> AppResult<()> {
    self.snapshot_control.queue_snapshot(params).await
  }

  async fn get_collab_snapshot(
    &self,
    workspace_id: &str,
    object_id: &str,
    snapshot_id: &i64,
  ) -> AppResult<SnapshotData> {
    self
      .snapshot_control
      .get_snapshot(workspace_id, object_id, snapshot_id)
      .await
  }

  async fn get_collab_snapshot_list(&self, oid: &str) -> AppResult<AFSnapshotMetas> {
    self.snapshot_control.get_collab_snapshot_list(oid).await
  }
}
