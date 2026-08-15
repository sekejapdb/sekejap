import type { RawDb, RawDbCtor } from 'sekejap/orm';
import SekejapJsiModule from './NativeSekejapJsi';

// Install the JSI HostObject once (idempotent). Must run before the first open.
let installed = false;
function ensureInstalled(): void {
  if (installed) return;
  if (!SekejapJsiModule.install()) {
    throw new Error('sekejap: JSI install failed — rebuild the app (New Architecture).');
  }
  installed = true;
}

// The React Native backend for the RawDb contract, backed by the sync JSI
// HostObject (`global.SekejapJSI`). Plugged into `Sekejap.open({ native })`.
export const SekejapJsi: RawDbCtor = {
  open(path: string): RawDb {
    ensureInstalled();
    const jsi = (globalThis as any).SekejapJSI;
    if (!jsi) throw new Error('sekejap: global.SekejapJSI missing after install.');
    return jsi.open(path) as RawDb;
  },
};
