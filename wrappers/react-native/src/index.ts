// sekejap for React Native — the same typed, reactive layer as Node, but sync
// over JSI. Re-exports the shared ergonomic API and adds the RN-native open().
export * from 'sekejap/orm';
export { useQuery } from 'sekejap/orm/react';

import { Sekejap } from 'sekejap/orm';
import type { Schema, Db } from 'sekejap/orm';
import { SekejapJsi } from './native';

/**
 * Open a database backed by the synchronous JSI native module.
 *
 * ```ts
 * const db = openSekejap('app.db', { schema: { dishes: Dish } });
 * const list = db.dishes.where(d => d.category.eq('main')).find(); // sync, like Node
 * ```
 */
export function openSekejap<S extends Schema>(
  path: string,
  opts: { schema: S },
): Db<S> {
  return Sekejap.open(path, { schema: opts.schema, native: SekejapJsi });
}
