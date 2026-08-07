// React / React Native binding — a hook over the reactive query.
// Import from 'sekejap/orm/react'. `react` is an optional peer dependency.
import { useEffect, useState } from 'react';
import type { Columns, Query } from './index';

/**
 * Subscribe a component to a query. Returns the current rows (undefined until the
 * first snapshot) and re-renders whenever a committed change touches the
 * collection. Pass a stable `deps` array when the query is rebuilt each render.
 *
 * ```tsx
 * const dishes = useQuery(db.dishes.where(d => d.category.eq('main')));
 * return <FlatList data={dishes ?? []} .../>;
 * ```
 */
export function useQuery<Row, C extends Columns>(
  query: Query<Row, C>,
  deps: unknown[] = [],
): Row[] | undefined {
  const [rows, setRows] = useState<Row[] | undefined>(undefined);
  useEffect(() => query.subscribe(setRows), deps);
  return rows;
}
