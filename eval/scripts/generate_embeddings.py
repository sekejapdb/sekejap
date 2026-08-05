import json, os, time, urllib.request, array
from concurrent.futures import ThreadPoolExecutor

import sys
D = os.environ.get("SEKEJAP_DATA", "./data") + "/prepared/search/fiqa"
KEY = os.environ["OPENROUTER_API_KEY"]  # embeddings: openai/text-embedding-3-small via OpenRouter
URL = "https://openrouter.ai/api/v1/embeddings"
MODEL = "openai/text-embedding-3-small"
BATCH = 100
WORKERS = 16

def load(path, tfn, maxchars=6000):
    ids, txts = [], []
    for line in open(path):
        if not line.strip(): continue
        o = json.loads(line); ids.append(o["_id"])
        t = tfn(o)[:maxchars]; txts.append(t if t.strip() else " ")
    return ids, txts

def embed_batch(texts, tries=6):
    body = json.dumps({"model": MODEL, "input": texts}).encode()
    for a in range(tries):
        try:
            req = urllib.request.Request(URL, body, {"Authorization": "Bearer " + KEY, "Content-Type": "application/json"})
            d = json.loads(urllib.request.urlopen(req, timeout=180).read())
            embs = [None] * len(texts)
            for e in d["data"]:
                embs[e["index"]] = e["embedding"]
            return embs
        except Exception as ex:
            if a == tries - 1:
                raise SystemExit(f"batch failed: {ex}")
            time.sleep(2 * (a + 1))

def run(name, ids, txts):
    batches = [txts[i:i+BATCH] for i in range(0, len(txts), BATCH)]
    results = [None] * len(batches)
    t0 = time.time(); done = [0]
    def work(k):
        r = embed_batch(batches[k]); done[0] += 1
        if done[0] % 50 == 0:
            print(f"  {name}: {done[0]}/{len(batches)} batches {time.time()-t0:.0f}s", flush=True)
        return k, r
    with ThreadPoolExecutor(WORKERS) as ex:
        for k, embs in ex.map(work, range(len(batches))):
            results[k] = embs
    flat = array.array('f')
    dim = len(results[0][0])
    for embs in results:
        for e in embs:
            flat.extend(e)
    open(f"{D}/{name}_emb.f32", "wb").write(flat.tobytes())
    open(f"{D}/{name}_ids.txt", "w").write("\n".join(ids))
    print(f"{name} DONE rows={len(ids)} dim={dim} {time.time()-t0:.0f}s", flush=True)

cids, ctxt = load(f"{D}/corpus.jsonl", lambda o: (o.get("title","")+" "+o.get("text","")).strip())
run("corpus", cids, ctxt)
qids, qtxt = load(f"{D}/queries.jsonl", lambda o: o["text"])
run("queries", qids, qtxt)
print("ALLDONE", flush=True)
