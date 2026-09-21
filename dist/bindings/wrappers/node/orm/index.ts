// sekejap/orm — the Node entry point. Re-exports the shared, backend-agnostic
// core and provides `Sekejap.open` defaulted to the Node napi backend, so
// backend code needs only `{ schema }`. The @sekejap/react-native package ships
// the same core with a JSI-defaulted `Sekejap` instead.
export * from './core';

import { open, type Schema, type Db } from './core';
import type { RawDbCtor } from './raw';
import { Native } from './native';

export const Sekejap = {
  /**
   * Open a database. Defaults to the Node napi backend, so backend code passes
   * only `{ schema }`. Pass `{ native }` to override (rarely needed on Node).
   */
  open<S extends Schema>(
    path: string,
    opts: { schema: S; native?: RawDbCtor },
  ): Db<S> {
    return open(path, { schema: opts.schema, native: opts.native ?? Native });
  },
};
