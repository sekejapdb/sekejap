// The Node (napi) backend for the RawDb contract. Node-only — it loads the
// native addon. React Native supplies its own JSI backend instead.
import type { RawDbCtor } from './raw';

// eslint-disable-next-line @typescript-eslint/no-var-requires
export const Native: RawDbCtor = require('../index.js').Db as RawDbCtor;

export type { RawDb, RawDbCtor } from './raw';
