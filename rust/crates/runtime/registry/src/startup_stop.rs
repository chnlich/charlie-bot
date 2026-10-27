use tokio::sync::watch;

/// A process-local startup stop request shared by startup steps.
#[derive(Clone)]
pub struct StartupStop {
  sender: watch::Sender<StartupPhase>,
  receiver: watch::Receiver<StartupPhase>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum StartupPhase {
  Starting,
  Stopped,
  Ready,
}

impl Default for StartupStop {
  fn default() -> Self {
    let (sender, receiver) = watch::channel(StartupPhase::Starting);
    Self { sender, receiver }
  }
}

impl StartupStop {
  /// Mark startup as stopped and wake any startup step waiting for the request.
  pub fn cancel(&self) {
    self.sender.send_if_modified(|phase| {
      if *phase == StartupPhase::Starting {
        *phase = StartupPhase::Stopped;
        true
      } else {
        false
      }
    });
  }

  /// Return whether startup has received a stop request.
  pub fn is_cancelled(&self) -> bool {
    *self.receiver.borrow() == StartupPhase::Stopped
  }

  /// Mark startup ready unless a stop request arrived first.
  pub fn mark_ready(&self) -> bool {
    let mut ready = false;
    self.sender.send_if_modified(|phase| match phase {
      StartupPhase::Starting => {
        *phase = StartupPhase::Ready;
        ready = true;
        true
      }
      StartupPhase::Ready => {
        ready = true;
        false
      }
      StartupPhase::Stopped => false,
    });
    ready
  }

  /// Wait until startup is ready, or return false when a stop request arrived first.
  pub async fn ready(&self) -> bool {
    let mut receiver = self.receiver.clone();
    loop {
      match *receiver.borrow() {
        StartupPhase::Ready => return true,
        StartupPhase::Stopped => return false,
        StartupPhase::Starting => {}
      }
      if receiver.changed().await.is_err() {
        return false;
      }
    }
  }

  /// Wait until startup receives a stop request.
  pub async fn cancelled(&self) {
    let mut receiver = self.receiver.clone();
    while *receiver.borrow() != StartupPhase::Stopped {
      receiver.changed().await.expect("the startup stop sender stays alive with its receiver");
    }
  }
}
