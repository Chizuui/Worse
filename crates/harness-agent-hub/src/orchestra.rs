//! Orchestra graph (phase-9-parity.md §9.11). The store setting `orchestra` holds the JSON; written only here.

use harness_core_api::{ApiError, ApiResult, Orchestra};

use crate::hub::Hub;
use crate::store;

const KEY: &str = "orchestra";

impl Hub {
    pub(crate) async fn do_get_orchestra(&self) -> ApiResult<Orchestra> {
        self.check_open()?;
        Ok(self.load_orchestra().await)
    }

    pub(crate) async fn do_set_orchestra(&self, orchestra: Orchestra) -> ApiResult<()> {
        self.check_open()?;
        orchestra.validate().map_err(ApiError::InvalidArgument)?;
        let json =
            serde_json::to_string(&orchestra).map_err(|e| ApiError::Internal(e.to_string()))?;
        self.ctx
            .store
            .run(move |c| store::set_setting(c, KEY, &json))
            .await?;
        Ok(())
    }

    /// The stored graph; empty when unset or unreadable.
    pub(crate) async fn load_orchestra(&self) -> Orchestra {
        self.ctx
            .store
            .run(|c| store::get_setting(c, KEY))
            .await
            .ok()
            .flatten()
            .and_then(|json| serde_json::from_str(&json).ok())
            .unwrap_or_default()
    }
}
