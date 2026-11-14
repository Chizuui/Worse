// Versions and the unique test product identity of the Phase 6 updater gate (phase6-update.mjs).
export const VERSION_N = "0.6.1";
export const VERSION_N1 = "0.6.2";
export const VERSION_DOWN = "0.6.0";
export const IDENTIFIER = "dev.harnesse2e.updater";
export const PRODUCT = "HarnessE2EUpdater";
export const BINARY = "harness-e2e-updater";
export const PUBLISHER = "Harness"; // tauri.conf.json bundle.publisher -> NSIS MANUFACTURER
export const UNINSTALL_KEY = `HKCU\\Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\${PRODUCT}`;
export const PRODUCT_KEY = `HKCU\\Software\\${PUBLISHER}\\${PRODUCT}`;
export const PUBLISHER_KEY = `HKCU\\Software\\${PUBLISHER}`;

export const RELEASE_NOTES = `# Harness ${VERSION_N1}\n\n- phase 6 updater gate build`;
