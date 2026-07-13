//! TUI lifecycle adapter for the daemon metrics hook.
//!
//! The composition root supplies reconnecting transport.  This adapter owns no
//! daemon state and makes disconnect/reconnect explicit so a TUI never treats
//! stale metrics as current.

use usagi_core::usecase::client::{ClientError, DaemonClient, DaemonRequest, MetricsAction};

/// The currently registered daemon observer, if any.
#[derive(Debug, Default)]
pub struct MetricsHook {
    registered: bool,
}

impl MetricsHook {
    /// Registers once when a TUI starts, or after a replacement connection.
    ///
    /// # Errors
    ///
    /// Returns the daemon transport or protocol error without marking the hook
    /// registered, so a later replacement connection can retry safely.
    pub fn connect<C: DaemonClient>(&mut self, client: &mut C) -> Result<(), ClientError> {
        if !self.registered {
            client.request(DaemonRequest::Metrics {
                action: MetricsAction::Subscribe,
            })?;
            self.registered = true;
        }
        Ok(())
    }

    /// Drops local registration knowledge after a transport loss.  The next
    /// successful connection always re-registers rather than resuming a stale
    /// connection-local subscriber.
    pub fn disconnected(&mut self) {
        self.registered = false;
    }

    /// Best-effort unregister during orderly TUI shutdown.
    ///
    /// # Errors
    ///
    /// Returns the daemon transport or protocol error. The caller can then
    /// close its connection; the daemon removes connection-local observers.
    pub fn shutdown<C: DaemonClient>(&mut self, client: &mut C) -> Result<(), ClientError> {
        if self.registered {
            client.request(DaemonRequest::Metrics {
                action: MetricsAction::Unsubscribe,
            })?;
            self.registered = false;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use usagi_core::usecase::client::{DaemonReply, MetricsAction};

    #[derive(Default)]
    struct Fake {
        requests: Vec<DaemonRequest>,
    }
    impl DaemonClient for Fake {
        fn request(&mut self, request: DaemonRequest) -> Result<DaemonReply, ClientError> {
            self.requests.push(request);
            Ok(DaemonReply::Ok(json!(null)))
        }
    }

    #[test]
    fn registers_unregisters_and_registers_again_after_disconnect() {
        let mut hook = MetricsHook::default();
        let mut client = Fake::default();
        hook.connect(&mut client).unwrap();
        hook.connect(&mut client).unwrap();
        hook.disconnected();
        hook.connect(&mut client).unwrap();
        hook.shutdown(&mut client).unwrap();
        assert_eq!(
            client.requests,
            vec![
                DaemonRequest::Metrics {
                    action: MetricsAction::Subscribe
                },
                DaemonRequest::Metrics {
                    action: MetricsAction::Subscribe
                },
                DaemonRequest::Metrics {
                    action: MetricsAction::Unsubscribe
                },
            ]
        );
    }
}
