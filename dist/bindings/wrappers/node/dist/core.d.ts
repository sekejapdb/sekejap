import type { RawDb, RawDbCtor } from './raw';
export type { RawDb, RawDbCtor } from './raw';
export interface GeoPoint {
    lon: number;
    lat: number;
}
type ColKind = 'text' | 'int' | 'real' | 'bool' | 'geo' | 'vector' | 'bm25' | 'json';
export interface ColumnDef<T> {
    readonly kind: ColKind;
    readonly _t?: T;
    readonly isKey?: boolean;
    readonly index?: 'btree' | 'hash';
    readonly dim?: number;
}
export declare const text: () => ColumnDef<string>;
export declare const int: () => ColumnDef<number>;
export declare const real: () => ColumnDef<number>;
export declare const bool: () => ColumnDef<boolean>;
export declare const json: <T = unknown>() => ColumnDef<T>;
export declare const geo: () => ColumnDef<GeoPoint>;
export declare const vector: (dim: number) => ColumnDef<number[]>;
export declare const bm25: (inner: ColumnDef<string>) => ColumnDef<string>;
export declare const key: <T>(c: ColumnDef<T>) => ColumnDef<T>;
export declare const index: <T>(c: ColumnDef<T>, kind?: "btree" | "hash") => ColumnDef<T>;
export type Columns = Record<string, ColumnDef<any>>;
export interface Entity<C extends Columns> {
    readonly name: string;
    readonly columns: C;
}
export declare const entity: <C extends Columns>(name: string, columns: C) => Entity<C>;
/** The row type inferred from an entity's columns. */
export type InferRow<E> = E extends Entity<infer C> ? {
    [K in keyof C]: C[K] extends ColumnDef<infer T> ? T : never;
} : never;
declare class SqlContext {
    readonly params: unknown[];
    ph(v: unknown): string;
    paramsJson(): string;
}
export declare abstract class Filter {
    abstract render(ctx: SqlContext): string;
    and(o: Filter): Filter;
    or(o: Filter): Filter;
}
/** A typed column reference — the `d.field` in `where(d => d.field.eq(x))`. */
export declare class Col<T> {
    readonly name: string;
    constructor(name: string);
    eq(v: T): Filter;
    neq(v: T): Filter;
    lt(v: T): Filter;
    lte(v: T): Filter;
    gt(v: T): Filter;
    gte(v: T): Filter;
    between(lo: T, hi: T): Filter;
}
export type ColRefs<C extends Columns> = {
    [K in keyof C]: C[K] extends ColumnDef<infer T> ? Col<T> : never;
};
export type VectorMetric = 'cosine' | 'dot';
export declare class Query<Row, C extends Columns> {
    private raw;
    private collection;
    private cols;
    private fromRow;
    private whereF;
    private extra;
    private order;
    private desc;
    private rankTerms;
    private lim;
    private off;
    constructor(raw: RawDb, collection: string, cols: ColRefs<C>, fromRow: (payload: any) => Row);
    where(build: (c: ColRefs<C>) => Filter): this;
    sortBy(select: (c: ColRefs<C>) => Col<any>, desc?: boolean): this;
    near(select: (c: ColRefs<C>) => Col<any>, p: GeoPoint, opts: {
        metres: number;
    }): this;
    matchText(select: (c: ColRefs<C>) => Col<string>, terms: string): this;
    rankByText(select: (c: ColRefs<C>) => Col<string>, terms: string, weight?: number, normalized?: boolean): this;
    rankByVector(select: (c: ColRefs<C>) => Col<number[]>, v: number[], metric?: VectorMetric, weight?: number): this;
    limit(n: number): this;
    offset(n: number): this;
    private clauses;
    private build;
    find(): Row[];
    findFirst(): Row | null;
    count(): number;
    update(assign: Partial<Row>): number;
    deleteAll(): number;
    /** Reactive (callback form): the current list now, then a fresh list after
     *  every commit that touches this collection. Returns an unsubscribe fn. */
    subscribe(onData: (rows: Row[]) => void): () => void;
    /** Reactive (async-iterable form): `for await (const rows of query.watch())`.
     *  Cleans up the native listener when the loop ends. */
    watch(): AsyncGenerator<Row[]>;
}
export declare class Collection<Row extends Record<string, any>, C extends Columns> {
    private raw;
    private ent;
    private cols;
    private keyCol;
    constructor(raw: RawDb, ent: Entity<C>);
    private fromRow;
    private toPayload;
    query(): Query<Row, C>;
    where(build: (c: ColRefs<C>) => Filter): Query<Row, C>;
    near(select: (c: ColRefs<C>) => Col<any>, p: GeoPoint, opts: {
        metres: number;
    }): Query<Row, C>;
    matchText(select: (c: ColRefs<C>) => Col<string>, terms: string): Query<Row, C>;
    all(): Query<Row, C>;
    find(): Row[];
    count(): number;
    put(o: Row): void;
    putAll(items: Row[]): void;
    get(k: string): Row | null;
    delete(k: string): void;
    /** Reactive over the whole collection (async-iterable). For a filtered watch,
     *  use `collection.where(...).watch()`. */
    watch(): AsyncGenerator<Row[], any, any>;
}
export type Schema = Record<string, Entity<any>>;
export type Db<S extends Schema> = SekejapBase & {
    [K in keyof S]: Collection<InferRow<S[K]>, S[K] extends Entity<infer C> ? C : never>;
};
declare class SekejapBase {
    readonly raw: RawDb;
    constructor(raw: RawDb);
    compact(): void;
}
/**
 * Open a database with an explicit backend. This core is backend-agnostic, so
 * `native` is required here — platform packages wrap this and inject their
 * default (`sekejap` → napi, `@sekejap/react-native` → JSI) as `Sekejap.open`,
 * so app code needs only `{ schema }`.
 */
export declare function open<S extends Schema>(path: string, opts: {
    schema: S;
    native: RawDbCtor;
}): Db<S>;
