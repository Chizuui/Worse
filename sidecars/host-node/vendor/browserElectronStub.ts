// Stand-in for the `electron` module inside the vendored ZCode browser owner (BrowserGuestManager).
// The manager only resolves guests through `webContents.fromId(id)`; in Harness every guest is a
// Chromium page target driven over CDP (src/browserGuest.ts) and registered here by id.
// `WebFrameMain` is a type-only import in the manager.

export interface StubGuestWebContents {
  readonly id: number;
  isDestroyed(): boolean;
}

const guests = new Map<number, StubGuestWebContents>();

/** Registers a CDP-backed guest under its synthetic webContents id; returns the unregister function. */
export function registerGuestWebContents(guest: StubGuestWebContents): () => void {
  guests.set(guest.id, guest);
  return () => {
    if (guests.get(guest.id) === guest) guests.delete(guest.id);
  };
}

export const webContents = {
  fromId(id: number): StubGuestWebContents | undefined {
    return guests.get(id);
  },
  getAllWebContents(): StubGuestWebContents[] {
    return Array.from(guests.values());
  },
};

export type WebFrameMain = unknown;

export default { webContents };
