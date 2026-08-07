// Typed TS layer end-to-end against the real napi addon.
//   node -e "..." rebuild addon first; then:  npx tsx orm/orm.test.ts
import assert from 'node:assert';
import { tmpdir } from 'node:os';
import { mkdtempSync } from 'node:fs';
import { join } from 'node:path';
import { Sekejap, entity, key, index, text, int, bool } from './index';

const Doc = entity('docs', {
  id: key(text()),
  name: text(),
  category: index(text(), 'hash'),
  value: index(int()), // btree
  openNow: bool(),
});

function open() {
  const dir = mkdtempSync(join(tmpdir(), 'sk_ts_'));
  return Sekejap.open(dir, { schema: { docs: Doc } });
}

// 1. typed put + get round-trip
{
  const db = open();
  db.docs.put({ id: 'd1', name: 'Nasi', category: 'main', value: 45000, openNow: true });
  const got = db.docs.get('d1');
  assert.ok(got, 'get returned null');
  assert.equal(got!.id, 'd1');
  assert.equal(got!.category, 'main');
  assert.equal(got!.value, 45000);
  assert.equal(got!.openNow, true);
  console.log('✓ typed put + get');
}

// 2. typed where / sortBy / count / range (btree flat-AND)
{
  const db = open();
  db.docs.putAll(
    Array.from({ length: 100 }, (_, i) => ({
      id: `k${i}`, name: `n${i}`, category: `cat${i % 10}`, value: i, openNow: i % 2 === 0,
    })),
  );
  const cheapMains = db.docs
    .where((d) => d.category.eq('cat3').and(d.value.lt(50)))
    .sortBy((d) => d.value)
    .find();
  assert.ok(cheapMains.every((d) => d.category === 'cat3' && d.value < 50));
  assert.ok(cheapMains.length > 0);

  const n = db.docs.where((d) => d.value.between(10, 20)).count();
  assert.equal(n, 11); // 10..20 inclusive
  console.log('✓ typed where / sortBy / count');
}

// 3. typed update + delete
{
  const db = open();
  db.docs.put({ id: 'x', name: 'n', category: 'c', value: 1, openNow: false });
  db.docs.where((d) => d.id.eq('x')).update({ value: 999 });
  assert.equal(db.docs.get('x')!.value, 999);
  db.docs.where((d) => d.id.eq('x')).deleteAll();
  assert.equal(db.docs.get('x'), null);
  console.log('✓ typed update + delete');
}

// 4. reactive subscribe re-emits on change
{
  const db = open();
  db.docs.put({ id: 'a', name: 'n', category: 'main', value: 1, openNow: true });
  const snapshots: number[] = [];
  const unsub = db.docs
    .where((d) => d.category.eq('main'))
    .subscribe((rows) => snapshots.push(rows.length));
  assert.equal(snapshots.length, 1); // initial
  assert.equal(snapshots[0], 1);
  db.docs.put({ id: 'b', name: 'n', category: 'main', value: 2, openNow: true }); // matching change
  // give the napi threadsafe callback a tick to fire
  setTimeout(() => {
    assert.ok(snapshots.length >= 2, `expected re-emit, got ${snapshots.length}`);
    assert.equal(snapshots[snapshots.length - 1], 2);
    unsub();
    console.log('✓ reactive subscribe re-emits');
    console.log('\nALL TS ORM TESTS PASSED');
  }, 100);
}
