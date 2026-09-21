import type { Columns, Query } from './core';
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
export declare function useQuery<Row, C extends Columns>(query: Query<Row, C>, deps?: unknown[]): Row[] | undefined;
