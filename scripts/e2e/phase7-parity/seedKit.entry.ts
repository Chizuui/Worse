// Gate-only bundle entry (built by phase7-parity into Worse/.work/tmp): ZCode's own e2e seed helpers,
// re-exported verbatim from the ZCode source. The Personal Provider config is written with ZCode's
// NodePersonalProviderConfigRepository exactly as ZCode's desktop e2e does before launching the app
// (packages/desktop/test/e2e/helpers/session-pane-retention-fixture.ts), so the "Use API key" sign-in
// wall never appears and no real account or cloud login is involved.
export {
  seedPersonalDefaultModelSelection,
  seedPersonalProviderConfig,
} from "../../../../packages/desktop/test/e2e/helpers/model-provider-restart-config.ts";
