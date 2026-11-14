import "./styles.css";
import { startHarnessDesktop } from "./host/bootstrap.js";
import { startHarnessMobile } from "./mobile/RemotePairing.js";
import { isMobileSurface } from "./platform/tauriPlatform.js";

// Phase 4: the phone is a web remote-control client only (no local Host, Agent, PTY, or file
// system); it pairs with a desktop and then loads the ZCode web remote page.
if (isMobileSurface) {
  startHarnessMobile();
} else {
  startHarnessDesktop();
}
