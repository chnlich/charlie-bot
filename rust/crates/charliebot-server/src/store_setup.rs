use std::collections::HashMap;
use std::sync::Arc;

use charliebot_core::metadata_slot::{MetadataFile, MetadataSlot, SlotField};
use charliebot_core::registry::Registry;
use charliebot_store::metadata::SessionMetadata;
use charliebot_store::{
  AccountLabelKeeper, AnchorFieldReconciler, SequenceListingFields, Store, StoreError, StoreHooks,
};
use serde_json::{Map, Value, json};

/// Build the startup metadata-slot registry and inject this server's current lifecycle hooks.
pub fn create(cfg: charliebot_core::config::CharlieBotConfig) -> Result<Store, String> {
  let mut registry = Registry::default();
  for slot in registered_slots() {
    registry.register_metadata_slot(slot)?;
  }
  let account_labels: Arc<dyn AccountLabelKeeper> = Arc::new(ClaudeAccountLabel);
  let anchor_reconciler: Arc<dyn AnchorFieldReconciler> =
    Arc::new(AnchorReconcile { account_labels: Arc::clone(&account_labels) });
  let sequence_listing: Arc<dyn SequenceListingFields> = Arc::new(EmptySequenceListing);
  Store::new(
    cfg,
    registry.metadata_slots().to_vec(),
    StoreHooks {
      account_labels: Some(account_labels),
      anchor_reconciler: Some(anchor_reconciler),
      sequence_listing: Some(sequence_listing),
    },
  )
  .map_err(|error| error.to_string())
}

fn registered_slots() -> Vec<MetadataSlot> {
  vec![
    MetadataSlot {
      owner: "backend_pool",
      on: MetadataFile::Session,
      after: None,
      fields: vec![SlotField { name: "backend_from_pool", default: json!(false) }],
    },
    MetadataSlot {
      owner: "cron",
      on: MetadataFile::Session,
      after: Some("backend"),
      fields: vec![
        SlotField { name: "scheduled_task", default: Value::Null },
        SlotField { name: "last_scheduled_run", default: Value::Null },
        SlotField { name: "last_run_status", default: Value::Null },
        SlotField { name: "last_scheduled_cron", default: Value::Null },
      ],
    },
    MetadataSlot {
      owner: "slack",
      on: MetadataFile::Session,
      after: Some("successor_session_id"),
      fields: vec![
        SlotField { name: "slack_origin", default: Value::Null },
        SlotField { name: "slack_watermark_ts", default: Value::Null },
      ],
    },
    MetadataSlot {
      owner: "discord",
      on: MetadataFile::Session,
      after: Some("successor_session_id"),
      fields: vec![
        SlotField { name: "discord_origin", default: Value::Null },
        SlotField { name: "discord_watermark_id", default: Value::Null },
      ],
    },
    MetadataSlot {
      owner: "web_reply",
      on: MetadataFile::Session,
      after: None,
      fields: vec![SlotField { name: "web_status", default: Value::Null }],
    },
    MetadataSlot {
      owner: "claude_code",
      on: MetadataFile::Session,
      after: Some("cc_session_started_at"),
      fields: vec![SlotField { name: "claude_account", default: Value::Null }],
    },
    MetadataSlot {
      owner: "claude_code",
      on: MetadataFile::Thread,
      after: Some("exit_code"),
      fields: vec![SlotField { name: "claude_session_id", default: Value::Null }],
    },
  ]
}

struct ClaudeAccountLabel;

impl AccountLabelKeeper for ClaudeAccountLabel {
  fn record_account_label(&self, meta: &mut SessionMetadata, label: Option<&str>) -> Result<bool, StoreError> {
    let value = label.map_or(Value::Null, |label| Value::String(label.to_string()));
    if meta.extra("claude_account") == Some(&value) {
      return Ok(false);
    }
    meta.set_extra("claude_account", value);
    Ok(true)
  }

  fn account_label(&self, meta: &SessionMetadata) -> Option<String> {
    meta.extra("claude_account").and_then(Value::as_str).map(str::to_string)
  }
}

struct AnchorReconcile {
  account_labels: Arc<dyn AccountLabelKeeper>,
}

impl AnchorFieldReconciler for AnchorReconcile {
  fn reconcile(&self, disk: &SessionMetadata, attempted: &mut SessionMetadata) -> Result<(), StoreError> {
    if attempted.cc_session_id != disk.cc_session_id {
      tracing::warn!(session_id = %attempted.id, field = "cc_session_id", "session_anchor_write_corrected");
      attempted.cc_session_id.clone_from(&disk.cc_session_id);
    }
    if attempted.native_backend != disk.native_backend {
      tracing::warn!(session_id = %attempted.id, field = "native_backend", "session_anchor_write_corrected");
      attempted.native_backend.clone_from(&disk.native_backend);
    }
    let disk_label = self.account_labels.account_label(disk);
    if self.account_labels.account_label(attempted) != disk_label {
      tracing::warn!(session_id = %attempted.id, field = "claude_account", "session_anchor_write_corrected");
      self.account_labels.record_account_label(attempted, disk_label.as_deref())?;
    }
    Ok(())
  }
}

struct EmptySequenceListing;

impl SequenceListingFields for EmptySequenceListing {
  fn owns_session(&self, _meta: &SessionMetadata) -> bool {
    false
  }

  fn listing_fields(
    &self,
    _session_ids: &[String],
    _now: charliebot_store::metadata::UtcDateTime,
  ) -> Result<HashMap<String, Map<String, Value>>, StoreError> {
    Ok(HashMap::new())
  }

  fn sequence_subtree_roots(&self, _metas: &[SessionMetadata]) -> Result<HashMap<String, String>, StoreError> {
    Ok(HashMap::new())
  }

  fn view_subtree_roots(
    &self,
    _metas: &[SessionMetadata],
  ) -> Result<HashMap<String, HashMap<String, String>>, StoreError> {
    Ok(HashMap::new())
  }

  fn derived_archived_ids(&self) -> Result<Vec<String>, StoreError> {
    Ok(Vec::new())
  }
}
