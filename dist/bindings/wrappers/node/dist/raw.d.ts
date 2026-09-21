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
