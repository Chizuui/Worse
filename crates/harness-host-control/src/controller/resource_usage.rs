//! Shell-initiated resource-usage snapshots of the Host process tree.

use std::time::Duration;

use harness_host_protocol::{resource_usage_cancel, resource_usage_request, ResourceUsageSnapshot};

use super::HostController;
use crate::error::HostControlError;
use crate::usage::UsageBegin;

impl HostController {
    /// Asks the Host for one resource-usage snapshot of its process tree.
    ///
    /// Follows Electron Main: at most one request is in flight; when it is
    /// still pending, or `timeout` elapses, the previous result (or an empty
    /// snapshot) is returned instead of queuing another request. Use
    /// [`harness_host_protocol::HOST_RESOURCE_USAGE_TIMEOUT`] for parity.
    ///
    /// # Errors
    ///
    /// Returns [`HostControlError::NotRunning`] when the Host is not ready or
    /// exits while sampling, and [`HostControlError::Io`] when stdin is closed.
    pub async fn resource_usage_snapshot(
        &self,
        timeout: Duration,
    ) -> Result<ResourceUsageSnapshot, HostControlError> {
        self.running_port()?;
        let (request_id, receiver) = match self.shared.usage().begin() {
            UsageBegin::Started {
                request_id,
                receiver,
            } => (request_id, receiver),
            UsageBegin::InFlight(last) => return Ok(last),
        };
        if let Err(error) = self
            .shared
            .send_host(resource_usage_request(&request_id))
            .await
        {
            self.shared.usage().abandon(&request_id);
            return Err(error);
        }
        match tokio::time::timeout(timeout, receiver).await {
            Ok(Ok(snapshot)) => Ok(snapshot),
            Ok(Err(_dropped)) => Err(HostControlError::NotRunning),
            // 与 Main 一致：超时只结束本次等待，请求保持在途，迟到的结果仍会刷新缓存。
            Err(_elapsed) => Ok(self.shared.usage().last_or_empty()),
        }
    }

    /// Ends the sampling session: forgets the cached snapshot and, when a
    /// request is in flight, tells the Host to cancel it.
    ///
    /// # Errors
    ///
    /// Returns [`HostControlError::Io`] when the cancellation cannot be written.
    pub async fn cancel_resource_usage(&self) -> Result<(), HostControlError> {
        let Some(request_id) = self.shared.usage().cancel() else {
            return Ok(());
        };
        if self.running_port().is_err() {
            return Ok(());
        }
        self.shared
            .send_host(resource_usage_cancel(&request_id))
            .await
    }
}
