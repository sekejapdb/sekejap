// The platform-agnostic native contract the typed layer lowers to. A backend
// (napi on Node, JSI on React Native) implements this; the ergonomic layer
// never imports a specific backend, so the same TS runs on both.

export interface RawDb {
  execute(sql: string): number;
  executeParams(sql: string, paramsJson: string): number;
  query(sql: string): string;
  queryParams(sql: string, paramsJson: string): string;
  put(slug: string, payloadJson: string): void;
  putMany(pairsJson: string): number;
  get(slug: string): string | null;
  remove(slug: string): void;
  compact(): void;
  watch(cb: (json: string) => void): number;
  unwatch(id: number): void;
}

export interface RawDbCtor {
  open(path: string): RawDb;
}
