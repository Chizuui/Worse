// Gate-only bundle entry (bundled by phase7-perf into its run dir): ZCode's own e2e seed helpers,
// re-exported from the ZCode source, so the Personal Provider config is written exactly as ZCode's
// desktop e2e writes it (no real account, no cloud login).
export {
  seedPersonalDefaultModelSelection,
  seedPersonalProviderConfig,
} from "../../../../packages/desktop/test/e2e/helpers/model-provider-restart-config.ts";
