// TypeScript definitions for the sekejap Node.js wrapper (over libsekejap's C ABI).
// See docs/dist/C_ABI.md for the contract this file mirrors.

export type SekejapStatusName =
  | 'Ok'
  | 'Refused'
  | 'Corrupt'
  | 'Unsupported'
  | 'Io'
  | 'Invalid'
  | 'Busy'
  | 'UnknownRow'
  | 'Unknown';

export const SekejapStatus: {
  readonly Ok: 0;
  readonly Refused: 1;
  readonly Corrupt: 2;
  readonly Unsupported: 3;
  readonly Io: 4;
  readonly Invalid: 5;
  readonly Busy: 6;
  readonly UnknownRow: 7;
  readonly Unknown: 8;
};

export const SekejapDirection: {
  readonly Outgoing: 0;
  readonly Incoming: 1;
  readonly Both: 2;
};

export type Direction = 'outgoing' | 'incoming' | 'both' | 0 | 1 | 2;

export class SekejapError extends Error {
  code: SekejapStatusName;
  status: number;
}

export type JsonValue =
  | null
  | boolean
  | number
  | string
  | JsonValue[]
  | { [key: string]: JsonValue };

export type Document = { _key?: string; [field: string]: JsonValue | undefined };

export type FieldKind = 'text' | 'int' | 'real' | 'bool' | 'json' | 'geo' | 'point' | 'vector';

export interface FieldDecl {
  name: string;
  kind: FieldKind;
  dimension?: number;
}

export interface IndexDecl {
  name: string;
  field: string;
  family: 'scalar' | 'text' | 'exact_vector' | 'quantized_vector' | 'spatial_point' | 'spatial_geometry';
  unique: boolean;
  ready: boolean;
}

export interface CollectionDescriptor {
  name: string;
  timestamps: boolean;
  rows: number | null;
  fields: Array<FieldDecl & { declared: string | null; primary_key: boolean }>;
  indexes: IndexDecl[];
}

export interface Neighbour {
  collection: string;
  key: string;
  document: Document;
}

export interface StoreConfig {
  budgetBytes?: number;
  io?: 'buffered' | 'direct';
  sync?: 'full' | 'normal' | 'off';
}

export interface Storage {
  dataBytes: number;
  walBytes: number;
  totalBytes: number;
}

export interface ChangeEvent {
  sequence: number;
  collections: string[];
  edge_types: string[];
  keys: Array<{ collection: string; key: string; kind: 'put' | 'delete' }>;
  keys_total: number;
  keys_truncated: boolean;
  unnamed_writes: number;
  rows_affected: number;
}

export class Scan {
  next(): JsonValue[] | null;
  close(): void;
  rows(): Generator<JsonValue, void, void>;
  [Symbol.iterator](): Generator<JsonValue[], void, void>;
}

export class Statement {
  query(params?: JsonValue): Document[];
  execute(params?: JsonValue): number;
  rebindable(): boolean | null;
  close(): void;
}

export class Tx {
  put(collection: string, key: string, doc: Document): void;
  delete(collection: string, key: string): boolean;
  link(fromCollection: string, fromKey: string, edgeType: string, toCollection: string, toKey: string): void;
  execute(sql: string, params?: JsonValue): number;
  commit(): void;
  rollback(): void;
}

export class Db {
  static open(path: string): Db;
  static openWithConfig(path: string, config?: StoreConfig): Db;
  static openService(path: string): Db;
  /** REFUSED by name: sekejap has no in-memory store. Always throws. */
  static openMemory(): never;

  close(): void;

  put(collection: string, key: string, doc: Document): void;
  putMany(collection: string, rows: Array<{ key: string; doc: Document }>): number;
  get(collection: string, key: string): Document | null;
  exists(collection: string, key: string): boolean;
  delete(collection: string, key: string): boolean;
  scan(collection: string, pageRows?: number): Scan;

  execute(sql: string, params?: JsonValue): number;
  query(sql: string, params?: JsonValue): Document[];
  explain(sql: string, params?: JsonValue): string;
  prepare(sql: string): Statement;
  stream(sql: string, params?: JsonValue, pageRows?: number): Scan;

  link(fromCollection: string, fromKey: string, edgeType: string, toCollection: string, toKey: string): void;
  linkWith(
    fromCollection: string,
    fromKey: string,
    edgeType: string,
    toCollection: string,
    toKey: string,
    properties: JsonValue
  ): void;
  unlink(fromCollection: string, fromKey: string, edgeType: string, toCollection: string, toKey: string): boolean;
  neighbours(
    collection: string,
    key: string,
    edgeType?: string | null,
    direction?: Direction,
    limit?: number
  ): Neighbour[];

  createCollection(name: string, fields: FieldDecl[]): boolean;
  dropCollection(name: string): boolean;
  collections(): string[];
  describe(collection: string): CollectionDescriptor | null;
  countRows(collection: string): number;
  scanCountRows(collection: string): number;
  scanCountEdges(): number;

  transaction(): Tx;

  checkpoint(): boolean;
  publish(): void;
  storage(): Storage;

  statementTimeoutMs(milliseconds: number): void;
  cancel(): void;
  clearInterrupt(): boolean;
  subscribe(): number;
  nextChange(subscriptionId: number, timeoutMs?: number): ChangeEvent | null;
  unsubscribe(subscriptionId: number): boolean;

  /** REFUSED: no proportional-to-rows memory to trim. Always throws. */
  trimMemory(): never;
  /** REFUSED: no payload-rewriting compaction. Always throws; use checkpoint(). */
  compact(): never;
  /** REFUSED: SHOW has no Tier-1 spelling. Always throws; use collections()/describe(). */
  show(statement?: string | null): never;
}

export function version(): string;
export function formatVersion(): number;
