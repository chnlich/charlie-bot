//! Broken-cron-file alerts: one Telegram message when the names of the failing files change, and the fingerprint
//! state that keeps the last alerted set, so a reload that finds the same broken files sends nothing.

use std::collections::HashSet;
use std::path::Path;
use std::sync::{Arc, Mutex};

use charliebot_core::config::CharlieBotConfig;
use charliebot_core::home::state_dir;
use charliebot_registry::context::FeatureContext;

use crate::config::ScheduledTaskError;

static CRON_ALERT_FINGERPRINT_LOCK: Mutex<()> = Mutex::new(());

pub(crate) fn fire_error_alert(features: &Arc<FeatureContext>, cfg: &CharlieBotConfig, errors: &[ScheduledTaskError]) {
  let names = errors.iter().map(|error| error.name.clone()).collect::<Vec<_>>();
  let path = state_dir(&cfg.charliebot_home).join("cron_alert_fingerprint.json");
  dispatch_alert_if_fingerprint_changed(&path, &names, |message| {
    if tokio::runtime::Handle::try_current().is_err() {
      tracing::info!(names = ?names, "cron_alert_skipped_no_event_loop");
      return false;
    }
    let credentials = Arc::clone(&features.credentials);
    let chat_id = cfg.telegram.chat_id.clone();
    tokio::spawn(async move {
      match send_telegram(&credentials, chat_id.as_deref(), &message).await {
        Ok(()) => tracing::info!("cron_alert_sent"),
        Err(error) => tracing::error!(error = %error, "cron_alert_send_failed"),
      }
    });
    true
  });
}

fn dispatch_alert_if_fingerprint_changed(path: &Path, names: &[String], dispatch: impl FnOnce(String) -> bool) {
  let _guard = CRON_ALERT_FINGERPRINT_LOCK.lock().expect("cron alert fingerprint mutex is not poisoned");
  let old = match std::fs::read_to_string(path) {
    Ok(raw) => match serde_json::from_str::<Vec<String>>(&raw) {
      Ok(names) => names.into_iter().collect::<HashSet<_>>(),
      Err(error) => {
        tracing::warn!(path = %path.display(), error = %error, "cron_alert_state_unparseable");
        HashSet::new()
      }
    },
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => HashSet::new(),
    Err(error) => {
      tracing::warn!(path = %path.display(), error = %error, "cron_alert_state_unreadable");
      HashSet::new()
    }
  };
  if old == names.iter().cloned().collect() {
    return;
  }
  if !dispatch(alert_message(names)) {
    return;
  }
  if let Err(error) = std::fs::create_dir_all(path.parent().expect("state path has a parent")).and_then(|()| {
    charliebot_core::fsutil::atomic_write_text(
      path,
      &format!("{}\n", serde_json::to_string(&names).expect("cron names serialize")),
    )
  }) {
    tracing::error!(path = %path.display(), error = %error, "cron_alert_state_write_failed");
  }
}

fn alert_message(names: &[String]) -> String {
  if names.is_empty() {
    "✅ all cron load failures resolved".to_string()
  } else {
    format!("⚠️ cron tasks failed to load: {}", names.join(", "))
  }
}

async fn send_telegram(
  credentials: &charliebot_core::hot_reload::HotReload<charliebot_core::credentials::Credentials>,
  chat_id: Option<&str>,
  message: &str,
) -> anyhow::Result<()> {
  let credentials = credentials.get()?;
  let token = credentials.require("telegram", charliebot_core::credentials::BOT_TOKEN_FIELD)?;
  let chat_id = chat_id.ok_or_else(|| anyhow::anyhow!("telegram.chat_id is not configured"))?;
  let response = charliebot_compat::http_client::shared_client()?
    .post(format!("https://api.telegram.org/bot{token}/sendMessage"))
    .json(&serde_json::json!({"chat_id": chat_id, "text": message.chars().take(4096).collect::<String>(), "parse_mode": "Markdown"}))
    .send()
    .await?;
  if response.status().is_success() {
    return Ok(());
  }
  let status = response.status();
  let body = match response.text().await {
    Ok(body) => body,
    Err(error) => {
      tracing::warn!(error = %error, "cron_alert_error_body_unreadable");
      String::new()
    }
  };
  anyhow::bail!("Telegram API returned {status}: {}", body.chars().take(200).collect::<String>())
}

#[cfg(test)]
mod tests {
  use super::{dispatch_alert_if_fingerprint_changed, fire_error_alert};
  use crate::test_support::feature_context;

  #[test]
  fn concurrent_alert_fingerprint_dispatches_once_per_error_set_transition() {
    use std::sync::{Arc, Barrier, Mutex};
    use std::thread;

    let home = tempfile::tempdir().unwrap();
    let state = home.path().join("state/cron_alert_fingerprint.json");
    let notifications = Arc::new(Mutex::new(Vec::new()));
    let barrier = Arc::new(Barrier::new(3));
    let names = vec!["alpha".to_string(), "beta".to_string()];
    let mut threads = Vec::new();
    for _ in 0..2 {
      let state = state.clone();
      let names = names.clone();
      let notifications = Arc::clone(&notifications);
      let barrier = Arc::clone(&barrier);
      threads.push(thread::spawn(move || {
        barrier.wait();
        dispatch_alert_if_fingerprint_changed(&state, &names, |message| {
          notifications.lock().unwrap().push(message);
          true
        });
      }));
    }
    barrier.wait();
    for thread in threads {
      thread.join().unwrap();
    }

    assert_eq!(*notifications.lock().unwrap(), ["⚠️ cron tasks failed to load: alpha, beta".to_string()]);
    assert_eq!(std::fs::read_to_string(&state).unwrap(), "[\"alpha\",\"beta\"]\n");

    for names in
      [vec!["beta".to_string(), "gamma".to_string()], Vec::new(), vec!["alpha".to_string(), "beta".to_string()]]
    {
      dispatch_alert_if_fingerprint_changed(&state, &names, |message| {
        notifications.lock().unwrap().push(message);
        true
      });
    }
    assert_eq!(
      *notifications.lock().unwrap(),
      [
        "⚠️ cron tasks failed to load: alpha, beta".to_string(),
        "⚠️ cron tasks failed to load: beta, gamma".to_string(),
        "✅ all cron load failures resolved".to_string(),
        "⚠️ cron tasks failed to load: alpha, beta".to_string(),
      ]
    );
  }

  #[test]
  fn alert_without_an_event_loop_does_not_persist_a_fingerprint() {
    let (_home, features) = feature_context();
    let cfg = features.config.get().unwrap();
    let error = crate::config::ScheduledTaskError {
      name: "synthetic".to_string(),
      path: "synthetic.yaml".to_string(),
      error: "synthetic load error".to_string(),
      enabled: Some(true),
    };
    fire_error_alert(&features, &cfg, &[error]);
    assert!(!cfg.charliebot_home.join("state/cron_alert_fingerprint.json").exists());
  }
}
