//! The quota panel's cache and poll loop (`src/features/usage/ext_usage.py`). The loop fetches one account per round-gap
//! sleep, in registration order. Each fetch rewrites that account's entry and broadcasts the whole panel on the sidebar
//! channel.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};
use std::time::Duration;

use charliebot_registry::usage::{PANEL_PROVIDER, PANEL_PROVIDER_LABEL, QuotaAccount};
use charliebot_store::metadata::UtcDateTime;
use futures_util::future::BoxFuture;
use serde_json::{Map, Value, json};

/// `timeouts.EXT_USAGE_ROUND_GAP_SECONDS`: the sleep after each fetch, and the sleep of a round with no accounts.
pub const EXT_USAGE_ROUND_GAP: Duration = Duration::from_secs(60);

/// The panel the poller writes and `GET /api/ext-usage` reads. Process-wide, as the module globals `_cached_usage` and
/// `_accounts` are in Python.
static PANEL: LazyLock<Arc<Panel>> = LazyLock::new(Arc::default);

/// The process-wide panel as a handle that a spawned task can own.
pub fn panel_handle() -> Arc<Panel> {
  Arc::clone(&PANEL)
}

/// The process-wide panel that the routes read.
pub fn panel() -> &'static Panel {
  &PANEL
}

/// What one round reads from outside the loop: the accounts the live config derives, the clock that judges expiry, and
/// the sidebar broadcast.
pub trait PollHost: Send + Sync {
  /// The quota accounts of the live config, in panel order.
  fn quota_accounts(&self) -> anyhow::Result<Vec<Arc<dyn QuotaAccount>>>;
  /// The time an emit judges expiry at.
  fn now(&self) -> UtcDateTime;
  /// A frame on the sidebar channel.
  fn broadcast_sidebar(&self, frame: Value) -> BoxFuture<'_, anyhow::Result<()>>;
}

/// The cache of entries keyed `<provider>:<label>`, and the accounts of the latest round.
#[derive(Default)]
pub struct Panel {
  state: Mutex<PanelState>,
}

#[derive(Default)]
struct PanelState {
  /// Entries in insertion order, which is the order of the broadcast and of the route body.
  cached: Map<String, Value>,
  /// The latest round's accounts by cache key. Every cached key has one.
  accounts: HashMap<String, Arc<dyn QuotaAccount>>,
}

impl Panel {
  fn lock(&self) -> MutexGuard<'_, PanelState> {
    self.state.lock().expect("the panel lock is never poisoned")
  }

  /// Adopts a round's accounts: prunes the entries of dropped accounts, then seeds a pending marker for each account not
  /// yet read, in derivation order.
  fn begin_round(&self, cycle: &[Arc<dyn QuotaAccount>]) {
    let mut state = self.lock();
    let PanelState { cached, accounts } = &mut *state;
    *accounts = cycle.iter().map(|account| (cache_key(account.as_ref()), Arc::clone(account))).collect();
    cached.retain(|key, _| accounts.contains_key(key));
    for account in cycle {
      cached
        .entry(cache_key(account.as_ref()))
        .or_insert_with(|| json!({PANEL_PROVIDER: account.provider(), "account": account.label(), "pending": true}));
    }
  }

  /// Writes one fetch's outcome. A reading replaces the entry. A failed fetch keeps a real reading, and otherwise writes
  /// the error entry. The account then sets or clears its login mark on the stored entry.
  fn record_fetch(&self, account: &dyn QuotaAccount, reading: Option<Map<String, Value>>) {
    let key = cache_key(account);
    let mut state = self.lock();
    let cached = &mut state.cached;
    match reading {
      Some(mut reading) => {
        reading.insert("account".to_string(), Value::String(account.label().to_string()));
        cached.insert(key.clone(), Value::Object(reading));
      }
      None => {
        let keeps_reading = cached
          .get(&key)
          .and_then(Value::as_object)
          .is_some_and(|previous| !previous.contains_key("error") && !previous.contains_key("pending"));
        if !keeps_reading {
          cached.insert(
            key.clone(),
            json!({PANEL_PROVIDER: account.provider(), "account": account.label(), "error": account.last_error()}),
          );
        }
      }
    }
    let entry = cached.get_mut(&key).and_then(Value::as_object_mut).expect("the round wrote this entry");
    account.mark_login_required(entry);
  }

  /// The entries as one emit shows them at `now`: each account's expiry marks judged on a copy, plus its provider's
  /// display label. Nothing is written back into the cache.
  pub fn annotated(&self, now: UtcDateTime) -> Map<String, Value> {
    let state = self.lock();
    state
      .cached
      .iter()
      .map(|(key, entry)| {
        let account = state.accounts.get(key).expect("every cached key has its round's account");
        let entry = entry.as_object().expect("a cache entry is an object");
        let mut marked = account.mark_expired(entry, now);
        marked.insert(PANEL_PROVIDER_LABEL.to_string(), Value::String(account.provider_label().to_string()));
        (key.clone(), Value::Object(marked))
      })
      .collect()
  }

  fn cache_keys(&self) -> Vec<String> {
    self.lock().cached.keys().cloned().collect()
  }
}

fn cache_key(account: &dyn QuotaAccount) -> String {
  format!("{}:{}", account.provider(), account.label())
}

/// `_poll_loop`: rounds forever. A round that fails logs once, then sleeps one round gap before the next round.
pub async fn poll_loop(panel: Arc<Panel>, host: Arc<dyn PollHost>) {
  loop {
    if let Err(error) = poll_round(&panel, host.as_ref()).await {
      tracing::error!(error = %format!("{error:#}"), "ext_usage_poll_error");
      tokio::time::sleep(EXT_USAGE_ROUND_GAP).await;
    }
  }
}

/// One round: the accounts are derived once, then each is fetched and broadcast in turn, with a sleep after each.
async fn poll_round(panel: &Panel, host: &dyn PollHost) -> anyhow::Result<()> {
  let cycle = host.quota_accounts()?;
  panel.begin_round(&cycle);
  if cycle.is_empty() {
    tokio::time::sleep(EXT_USAGE_ROUND_GAP).await;
    return Ok(());
  }
  for account in &cycle {
    let reading = match account.fetch().await {
      Ok(reading) => reading,
      Err(error) => {
        tracing::error!(
          provider = account.provider(),
          account = account.label(),
          error = %format!("{error:#}"),
          "ext_usage_poll_error"
        );
        None
      }
    };
    panel.record_fetch(account.as_ref(), reading);
    host.broadcast_sidebar(json!({"type": "ext_usage", "providers": panel.annotated(host.now())})).await?;
    tracing::info!(providers = ?panel.cache_keys(), "ext_usage_fetched");
    tokio::time::sleep(EXT_USAGE_ROUND_GAP).await;
  }
  Ok(())
}

#[cfg(test)]
mod tests {
  use std::collections::VecDeque;
  use std::future::Future;
  use std::pin::Pin;
  use std::sync::atomic::{AtomicUsize, Ordering};

  use charliebot_registry::usage::{PANEL_EXPIRED, PANEL_RESETS_AT, PANEL_WINDOWS};
  use tokio::time::Instant;

  use super::*;

  type Outcome = Result<Option<Map<String, Value>>, String>;

  /// A quota account whose fetches follow a script. Its `mark_expired` marks each window whose reset has passed.
  struct StubAccount {
    provider: &'static str,
    provider_label: &'static str,
    label: &'static str,
    last_error: &'static str,
    script: Mutex<VecDeque<Outcome>>,
  }

  impl QuotaAccount for StubAccount {
    fn provider(&self) -> &str {
      self.provider
    }

    fn provider_label(&self) -> &str {
      self.provider_label
    }

    fn label(&self) -> &str {
      self.label
    }

    fn last_error(&self) -> String {
      self.last_error.to_string()
    }

    fn fetch<'a>(&'a self) -> BoxFuture<'a, anyhow::Result<Option<Map<String, Value>>>> {
      let outcome = self.script.lock().unwrap().pop_front().expect("the script covers every fetch");
      Box::pin(async move { outcome.map_err(anyhow::Error::msg) })
    }

    fn mark_expired(&self, entry: &Map<String, Value>, now: UtcDateTime) -> Map<String, Value> {
      let mut marked = entry.clone();
      if let Some(Value::Array(windows)) = marked.get_mut(PANEL_WINDOWS) {
        for window in windows {
          let resets_at = window.get(PANEL_RESETS_AT).and_then(Value::as_str).filter(|reset| !reset.is_empty());
          if resets_at.is_some_and(|reset| UtcDateTime::parse(reset).unwrap() <= now) {
            window.as_object_mut().unwrap().insert(PANEL_EXPIRED.to_string(), Value::Bool(true));
          }
        }
      }
      marked
    }
  }

  fn account(
    provider: &'static str,
    provider_label: &'static str,
    label: &'static str,
    last_error: &'static str,
    script: Vec<Outcome>,
  ) -> Arc<dyn QuotaAccount> {
    Arc::new(StubAccount { provider, provider_label, label, last_error, script: Mutex::new(script.into()) })
  }

  fn reading(utilization: f64, resets_at: &str) -> Outcome {
    let window = json!({"window_minutes": 300, "utilization": utilization, "resets_at": resets_at});
    Ok(Some(
      json!({"windows": [window], "fetched_at": "2026-01-01T00:00:00+00:00", "provider": "claude"})
        .as_object()
        .unwrap()
        .clone(),
    ))
  }

  type Derive = dyn Fn(usize) -> anyhow::Result<Vec<Arc<dyn QuotaAccount>>> + Send + Sync;

  /// The poll host: accounts from a function of the round's index, a settable clock, and the frames it received.
  struct StubHost {
    derive: Box<Derive>,
    derivations: AtomicUsize,
    now: Mutex<UtcDateTime>,
    frames: Mutex<Vec<Value>>,
  }

  impl StubHost {
    fn new(
      derive: impl Fn(usize) -> anyhow::Result<Vec<Arc<dyn QuotaAccount>>> + Send + Sync + 'static,
      now: &str,
    ) -> Arc<Self> {
      Arc::new(Self {
        derive: Box::new(derive),
        derivations: AtomicUsize::new(0),
        now: Mutex::new(UtcDateTime::parse(now).unwrap()),
        frames: Mutex::new(Vec::new()),
      })
    }

    fn frames(&self) -> Vec<Value> {
      self.frames.lock().unwrap().clone()
    }

    fn derivations(&self) -> usize {
      self.derivations.load(Ordering::SeqCst)
    }
  }

  impl PollHost for StubHost {
    fn quota_accounts(&self) -> anyhow::Result<Vec<Arc<dyn QuotaAccount>>> {
      let round = self.derivations.fetch_add(1, Ordering::SeqCst);
      (self.derive)(round)
    }

    fn now(&self) -> UtcDateTime {
      *self.now.lock().unwrap()
    }

    fn broadcast_sidebar(&self, frame: Value) -> BoxFuture<'_, anyhow::Result<()>> {
      self.frames.lock().unwrap().push(frame);
      Box::pin(async { Ok(()) })
    }
  }

  type PollFuture = Pin<Box<dyn Future<Output = ()> + Send>>;

  /// The loop, not yet started. Each `run_until` advances it to one instant on the paused clock.
  fn start_loop(panel: &Arc<Panel>, host: &Arc<StubHost>) -> PollFuture {
    Box::pin(poll_loop(Arc::clone(panel), Arc::clone(host) as Arc<dyn PollHost>))
  }

  /// Runs the loop until `at` after `start`. The loop never ends, so the timeout must fire.
  async fn run_until(poll: &mut PollFuture, start: Instant, at: Duration) {
    let remaining = (start + at).saturating_duration_since(Instant::now());
    assert!(tokio::time::timeout(remaining, poll.as_mut()).await.is_err(), "the poll loop never ends");
  }

  fn providers(frame: &Value) -> &Map<String, Value> {
    frame["providers"].as_object().expect("a frame carries the providers map")
  }

  fn keys(map: &Map<String, Value>) -> Vec<&str> {
    map.keys().map(String::as_str).collect()
  }

  #[tokio::test(start_paused = true)]
  async fn a_round_seeds_pending_markers_in_derivation_order_and_broadcasts_each_fetch() {
    let main = account("claude", "Claude", "main", "no data", vec![reading(1.0, "")]);
    let ext = account("claude", "Claude", "ext-1", "credentials not found", vec![Ok(None)]);
    let codex = account("codex", "Codex", "main", "no data", vec![Ok(Some(Map::new()))]);
    let host =
      StubHost::new(move |_| Ok(vec![Arc::clone(&main), Arc::clone(&ext), Arc::clone(&codex)]), "2026-01-01T00:00:00Z");
    let panel = Arc::new(Panel::default());
    let mut poll = start_loop(&panel, &host);
    let start = Instant::now();

    run_until(&mut poll, start, Duration::from_secs(150)).await;

    let frames = host.frames();
    assert_eq!(frames.len(), 3);
    for frame in &frames {
      assert_eq!(keys(frame.as_object().unwrap()), ["type", "providers"]);
      assert_eq!(frame["type"], "ext_usage");
    }
    assert_eq!(keys(providers(&frames[0])), ["claude:main", "claude:ext-1", "codex:main"]);
    assert_eq!(
      providers(&frames[0])["claude:ext-1"],
      json!({"provider": "claude", "account": "ext-1", "pending": true, "provider_label": "Claude"})
    );
    assert_eq!(
      providers(&frames[0])["codex:main"],
      json!({"provider": "codex", "account": "main", "pending": true, "provider_label": "Codex"})
    );
    assert_eq!(
      providers(&frames[1])["claude:ext-1"],
      json!({"provider": "claude", "account": "ext-1", "error": "credentials not found", "provider_label": "Claude"})
    );
    assert_eq!(keys(providers(&frames[2])), ["claude:main", "claude:ext-1", "codex:main"]);
    assert_eq!(providers(&frames[2])["codex:main"]["account"], "main");
  }

  #[tokio::test(start_paused = true)]
  async fn a_dropped_account_is_pruned_when_the_next_round_no_longer_lists_it() {
    let main = account("claude", "Claude", "main", "no data", vec![reading(1.0, ""), reading(3.0, "")]);
    let gone = account("claude", "Claude", "gone", "no data", vec![reading(2.0, "")]);
    let host = StubHost::new(
      move |round| Ok(if round == 0 { vec![Arc::clone(&main), Arc::clone(&gone)] } else { vec![Arc::clone(&main)] }),
      "2026-01-01T00:00:00Z",
    );
    let panel = Arc::new(Panel::default());
    let mut poll = start_loop(&panel, &host);
    let start = Instant::now();

    run_until(&mut poll, start, Duration::from_secs(150)).await;

    assert_eq!(keys(&panel.lock().cached), ["claude:main"]);
    assert_eq!(keys(providers(&host.frames()[2])), ["claude:main"]);
  }

  #[tokio::test(start_paused = true)]
  async fn a_failed_fetch_after_a_real_reading_keeps_the_reading() {
    let main = account("claude", "Claude", "main", "rate limited", vec![reading(42.0, ""), Ok(None)]);
    let host = StubHost::new(move |_| Ok(vec![Arc::clone(&main)]), "2026-01-01T00:00:00Z");
    let panel = Arc::new(Panel::default());
    let mut poll = start_loop(&panel, &host);
    let start = Instant::now();

    run_until(&mut poll, start, Duration::from_secs(90)).await;

    let frames = host.frames();
    assert_eq!(frames.len(), 2);
    let state = panel.lock();
    let kept = &state.cached["claude:main"];
    assert_eq!(kept["windows"][0]["utilization"], 42.0);
    assert_eq!(kept["account"], "main");
    assert!(kept.get("error").is_none());
    assert_eq!(providers(&frames[1])["claude:main"]["windows"][0]["utilization"], 42.0);
  }

  #[tokio::test(start_paused = true)]
  async fn a_failed_fetch_after_a_pending_marker_writes_the_error_entry() {
    let main = account("claude", "Claude", "main", "no data", vec![reading(1.0, "")]);
    let ext = account("claude", "Claude", "ext-1", "rate limited", vec![Ok(None)]);
    let host = StubHost::new(move |_| Ok(vec![Arc::clone(&main), Arc::clone(&ext)]), "2026-01-01T00:00:00Z");
    let panel = Arc::new(Panel::default());
    let mut poll = start_loop(&panel, &host);
    let start = Instant::now();

    run_until(&mut poll, start, Duration::from_secs(90)).await;

    let frames = host.frames();
    assert_eq!(
      providers(&frames[0])["claude:ext-1"],
      json!({"provider": "claude", "account": "ext-1", "pending": true, "provider_label": "Claude"})
    );
    assert_eq!(
      providers(&frames[1])["claude:ext-1"],
      json!({"provider": "claude", "account": "ext-1", "error": "rate limited", "provider_label": "Claude"})
    );
  }

  #[tokio::test(start_paused = true)]
  async fn a_quota_accounts_error_logs_once_and_sleeps_the_round_gap_before_the_next_round() {
    let main = account("claude", "Claude", "main", "no data", vec![reading(1.0, "")]);
    let host = StubHost::new(
      move |round| match round {
        0 => Err(anyhow::anyhow!("config is unreadable")),
        _ => Ok(vec![Arc::clone(&main)]),
      },
      "2026-01-01T00:00:00Z",
    );
    let panel = Arc::new(Panel::default());
    let mut poll = start_loop(&panel, &host);
    let start = Instant::now();

    run_until(&mut poll, start, Duration::from_secs(30)).await;
    assert_eq!((host.derivations(), host.frames().len()), (1, 0));

    run_until(&mut poll, start, Duration::from_secs(90)).await;
    assert_eq!((host.derivations(), host.frames().len()), (2, 1));
    assert_eq!(keys(&panel.lock().cached), ["claude:main"]);
  }

  #[tokio::test(start_paused = true)]
  async fn a_round_with_no_accounts_sleeps_once_and_broadcasts_nothing() {
    let host = StubHost::new(|_| Ok(Vec::new()), "2026-01-01T00:00:00Z");
    let panel = Arc::new(Panel::default());
    let mut poll = start_loop(&panel, &host);
    let start = Instant::now();

    run_until(&mut poll, start, Duration::from_secs(30)).await;
    assert_eq!(host.derivations(), 1);

    run_until(&mut poll, start, Duration::from_secs(150)).await;
    assert_eq!(host.derivations(), 3);
    assert!(host.frames().is_empty());
    assert!(panel.annotated(UtcDateTime::now()).is_empty());
  }

  #[tokio::test(start_paused = true)]
  async fn expiry_marks_are_judged_at_each_emit_and_never_written_into_the_cache() {
    let main = account(
      "claude",
      "Claude",
      "main",
      "no data",
      vec![reading(1.0, "2026-01-01T03:00:00+00:00"), reading(1.0, "2026-01-01T03:00:00+00:00")],
    );
    let host = StubHost::new(move |_| Ok(vec![Arc::clone(&main)]), "2026-01-01T02:00:00Z");
    let panel = Arc::new(Panel::default());
    let mut poll = start_loop(&panel, &host);
    let start = Instant::now();

    run_until(&mut poll, start, Duration::from_secs(30)).await;
    *host.now.lock().unwrap() = UtcDateTime::parse("2026-01-01T04:00:00Z").unwrap();
    run_until(&mut poll, start, Duration::from_secs(90)).await;

    let frames = host.frames();
    assert!(providers(&frames[0])["claude:main"]["windows"][0].get(PANEL_EXPIRED).is_none());
    assert_eq!(providers(&frames[1])["claude:main"]["windows"][0][PANEL_EXPIRED], true);
    assert!(panel.lock().cached["claude:main"]["windows"][0].get(PANEL_EXPIRED).is_none());
  }
}
