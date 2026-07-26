//! Per-record compression under a HARD 1-record blast radius — pushed to ceiling.
//!
//! Every mode is per-record-independent: a corrupt byte kills exactly ONE record
//! (same as raw). We push schema-aware binary as far as it goes while staying
//! per-record and roundtrip-exact:
//!   - field names -> int IDs
//!   - typed values (varint ints, packed bool/null, f64)
//!   - value interning (low-cardinality strings -> IDs)
//!   - per-field prefix/suffix TEMPLATES (cust-000123 -> 000123; constant date
//!     framing stripped) — the general, exact way to shrink "unique" strings
//! vs raw and per-record zstd. Roundtrip verified (zero loss) before timing.
//!
//!   cargo run --release --example schema_encode_bench

use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::time::Instant;

fn env(n: &str, d: usize) -> usize { std::env::var(n).ok().and_then(|v| v.parse().ok()).unwrap_or(d) }
fn mb(b: usize) -> f64 { b as f64 / (1024.0 * 1024.0) }

const WORDS: [&str; 16] = ["delivered","pending","review","approved","shipment","customer","priority","standard","handling","warehouse","return","refund","invoice","tracking","confirmed","processing"];
fn gen(k: usize) -> String {
    let w = |i: usize| WORDS[(k.wrapping_mul(2654435761).wrapping_add(i)) % WORDS.len()];
    format!(
        r#"{{"_collection":"order","_key":"ord-{k:08x}","customer_id":"cust-{:06}","email":"user{}@example.com","sku":"SKU-{:05}-{}","quantity":{},"unit_price_cents":{},"currency":"USD","status":"{}","warehouse":"wh-{}-{}","carrier":"{}","created_at":"2026-{:02}-{:02}T{:02}:{:02}:00Z","notes":"Order {} is {} at the {} facility; {} handling requested.","tags":["{}","{}","{}"],"attempt":{}}}"#,
        k%900000, k%100000, k%90000, ["A","B","C","D"][k%4], (k%5)+1, (k%5000)*10+99,
        w(0), (k%9)+1, ["east","west","north","south"][k%4], w(1),
        (k%12)+1, (k%27)+1, k%24, k%60, k, w(2), w(3), w(4),
        w(5), w(6), w(7), (k%3)+1
    )
}

fn put_uv(o: &mut Vec<u8>, mut v: u64) { loop { let b=(v&0x7f) as u8; v>>=7; if v==0 { o.push(b); break } o.push(b|0x80) } }
fn get_uv(b: &[u8], p: &mut usize) -> u64 { let mut v=0u64; let mut s=0; loop { let x=b[*p]; *p+=1; v|=((x&0x7f) as u64)<<s; if x&0x80==0 { break } s+=7 } v }
fn zig(v: i64) -> u64 { ((v<<1)^(v>>63)) as u64 }
fn unzig(v: u64) -> i64 { ((v>>1) as i64)^-((v&1) as i64) }

struct Dicts {
    field_id: HashMap<String, u64>, fields: Vec<String>,
    val_id: HashMap<String, u64>, vals: Vec<String>,
    tmpl: HashMap<u64, (String, String)>, // field_id -> (prefix, suffix)
}

// Generic value encode (nested/arrays; no templates here).
fn enc_val(v: &Value, d: &Dicts, o: &mut Vec<u8>) {
    match v {
        Value::Null => o.push(0),
        Value::Bool(false) => o.push(1),
        Value::Bool(true) => o.push(2),
        Value::Number(n) => { if let Some(i)=n.as_i64() { o.push(3); put_uv(o, zig(i)); } else { o.push(4); o.extend_from_slice(&n.as_f64().unwrap().to_le_bytes()); } }
        Value::String(s) => { if let Some(&id)=d.val_id.get(s) { o.push(5); put_uv(o,id); } else { o.push(6); put_uv(o,s.len() as u64); o.extend_from_slice(s.as_bytes()); } }
        Value::Array(a) => { o.push(7); put_uv(o,a.len() as u64); for e in a { enc_val(e,d,o); } }
        Value::Object(m) => { o.push(8); put_uv(o,m.len() as u64); for (k,val) in m { put_uv(o,d.field_id[k]); enc_val(val,d,o); } }
    }
}
// Top-level record encode: applies per-field templates to string values.
fn enc_record(v: &Value, d: &Dicts, o: &mut Vec<u8>) {
    let m = v.as_object().unwrap();
    o.push(8); put_uv(o, m.len() as u64);
    for (k, val) in m {
        let fid = d.field_id[k]; put_uv(o, fid);
        if let (Value::String(s), Some((pre, suf))) = (val, d.tmpl.get(&fid)) {
            if s.len() >= pre.len()+suf.len() && s.starts_with(pre.as_str()) && s.ends_with(suf.as_str()) {
                let mid = &s[pre.len()..s.len()-suf.len()];
                o.push(9);
                if let Some(&id)=d.val_id.get(mid) { o.push(0); put_uv(o,id); }
                else { o.push(1); put_uv(o,mid.len() as u64); o.extend_from_slice(mid.as_bytes()); }
                continue;
            }
        }
        enc_val(val, d, o);
    }
}
fn dec_val(b: &[u8], p: &mut usize, d: &Dicts) -> Value {
    let tag=b[*p]; *p+=1;
    match tag {
        0=>Value::Null, 1=>Value::Bool(false), 2=>Value::Bool(true),
        3=>Value::Number(unzig(get_uv(b,p)).into()),
        4=>{ let mut x=[0u8;8]; x.copy_from_slice(&b[*p..*p+8]); *p+=8; serde_json::json!(f64::from_le_bytes(x)) }
        5=>Value::String(d.vals[get_uv(b,p) as usize].clone()),
        6=>{ let l=get_uv(b,p) as usize; let s=String::from_utf8(b[*p..*p+l].to_vec()).unwrap(); *p+=l; Value::String(s) }
        7=>{ let n=get_uv(b,p) as usize; Value::Array((0..n).map(|_| dec_val(b,p,d)).collect()) }
        8=>{ let n=get_uv(b,p) as usize; let mut m=serde_json::Map::new(); for _ in 0..n { let f=get_uv(b,p) as usize; let k=d.fields[f].clone(); m.insert(k, dec_field(b,p,d,f as u64)); } Value::Object(m) }
        _=>unreachable!(),
    }
}
// Decode a value that may be a templated string (tag 9) given its field id.
fn dec_field(b: &[u8], p: &mut usize, d: &Dicts, fid: u64) -> Value {
    if b[*p]==9 {
        *p+=1; let sub=b[*p]; *p+=1;
        let mid = if sub==0 { d.vals[get_uv(b,p) as usize].clone() } else { let l=get_uv(b,p) as usize; let s=String::from_utf8(b[*p..*p+l].to_vec()).unwrap(); *p+=l; s };
        let (pre,suf)=&d.tmpl[&fid];
        Value::String(format!("{pre}{mid}{suf}"))
    } else { dec_val(b,p,d) }
}
fn dec_record(b: &[u8], d: &Dicts) -> Value {
    let mut p=0; let tag=b[p]; p+=1; assert_eq!(tag,8);
    let n=get_uv(b,&mut p) as usize; let mut m=serde_json::Map::new();
    for _ in 0..n { let f=get_uv(b,&mut p); let k=d.fields[f as usize].clone(); m.insert(k, dec_field(b,&mut p,d,f)); }
    Value::Object(m)
}
fn skip_val(b: &[u8], p: &mut usize) {
    let tag=b[*p]; *p+=1;
    match tag {
        0|1|2=>{}, 3=>{get_uv(b,p);}, 4=>{*p+=8;}, 5=>{get_uv(b,p);},
        6=>{let l=get_uv(b,p) as usize; *p+=l;},
        7=>{let n=get_uv(b,p) as usize; for _ in 0..n { skip_val(b,p); }},
        8=>{let n=get_uv(b,p) as usize; for _ in 0..n { get_uv(b,p); skip_field(b,p); }},
        9=>{let sub=b[*p]; *p+=1; if sub==0 { get_uv(b,p); } else { let l=get_uv(b,p) as usize; *p+=l; }},
        _=>unreachable!(),
    }
}
fn skip_field(b: &[u8], p: &mut usize) { if b[*p]==9 { skip_val(b,p); } else { skip_val(b,p); } }
fn get_field(b: &[u8], fid: u64, d: &Dicts) -> Option<Value> {
    let mut p=0; if b[p]!=8 { return None; } p+=1;
    let n=get_uv(b,&mut p) as usize;
    for _ in 0..n { let f=get_uv(b,&mut p); if f==fid { return Some(dec_field(b,&mut p,d,f)); } skip_field(b,&mut p); }
    None
}

fn lcp(a: &str, b: &str) -> usize { a.bytes().zip(b.bytes()).take_while(|(x,y)| x==y).count().min(a.len()).min(b.len()) }
fn lcs(a: &str, b: &str) -> usize { a.bytes().rev().zip(b.bytes().rev()).take_while(|(x,y)| x==y).count().min(a.len()).min(b.len()) }

fn main() {
    let n = env("NRECORDS", 200_000);
    let recs: Vec<String> = (0..n).map(gen).collect();
    let parsed: Vec<Value> = recs.iter().map(|s| serde_json::from_str(s).unwrap()).collect();
    let raw_total: usize = recs.iter().map(|s| s.len()).sum();
    println!("== per-record compression under 1-record blast radius (ceiling) ==");
    println!("records={n}  avg {} B  raw {:.1} MB\n", raw_total/n, mb(raw_total));

    let mut d = Dicts { field_id:HashMap::new(), fields:vec![], val_id:HashMap::new(), vals:vec![], tmpl:HashMap::new() };
    // field dict (top-level + nested keys)
    let intern = |m:&mut HashMap<String,u64>, v:&mut Vec<String>, k:&str| { if !m.contains_key(k){ m.insert(k.to_string(), v.len() as u64); v.push(k.to_string()); } };
    for p in &parsed { if let Value::Object(o)=p { for (k,val) in o { intern(&mut d.field_id,&mut d.fields,k); if let Value::Object(no)=val { for (nk,_) in no { intern(&mut d.field_id,&mut d.fields,nk); } } } } }

    // per-field prefix/suffix templates (top-level string-only fields)
    let mut fstr: HashMap<u64, Vec<&str>> = HashMap::new();
    let mut fbad: HashSet<u64> = HashSet::new();
    for p in &parsed { if let Value::Object(o)=p { for (k,val) in o { let fid=d.field_id[k]; if let Value::String(s)=val { fstr.entry(fid).or_default().push(s); } else { fbad.insert(fid); } } } }
    for (fid, vs) in &fstr {
        if fbad.contains(fid) || vs.len()<2 { continue; }
        let mut pre = vs[0].to_string(); let mut suf = vs[0].to_string();
        for s in &vs[1..] { pre.truncate(lcp(&pre,s)); let l=lcs(&suf,s); let start=suf.len()-l; suf=suf[start..].to_string(); }
        // don't let prefix+suffix overlap the shortest value
        let minlen = vs.iter().map(|s| s.len()).min().unwrap();
        while pre.len()+suf.len()>minlen && !pre.is_empty() { pre.pop(); }
        while pre.len()+suf.len()>minlen && !suf.is_empty() { let s=suf[1..].to_string(); suf=s; }
        if pre.len()+suf.len()>=4 { d.tmpl.insert(*fid, (pre, suf)); }
    }

    // intern string values (use MIDDLES for templated fields), freq>=8
    let mut vf: HashMap<String,u32> = HashMap::new();
    fn collect(v:&Value, fid:Option<u64>, d:&Dicts, vf:&mut HashMap<String,u32>) {
        match v {
            Value::String(s)=>{ let key = if let Some(f)=fid { if let Some((pre,suf))=d.tmpl.get(&f) { if s.len()>=pre.len()+suf.len() && s.starts_with(pre.as_str()) && s.ends_with(suf.as_str()) { s[pre.len()..s.len()-suf.len()].to_string() } else { s.clone() } } else { s.clone() } } else { s.clone() }; *vf.entry(key).or_default()+=1; }
            Value::Array(a)=>for e in a { collect(e,None,d,vf); }
            Value::Object(o)=>for (k,val) in o { collect(val, d.field_id.get(k).copied(), d, vf); }
            _=>{}
        }
    }
    for p in &parsed { collect(p,None,&d,&mut vf); }
    for (s,f) in &vf { if *f>=8 && s.len()>=2 { d.val_id.insert(s.clone(), d.vals.len() as u64); d.vals.push(s.clone()); } }

    let dict_bytes: usize = d.fields.iter().map(|s| s.len()+2).sum::<usize>()
        + d.vals.iter().map(|s| s.len()+2).sum::<usize>()
        + d.tmpl.values().map(|(a,b)| a.len()+b.len()+4).sum::<usize>();
    println!("  shared tables: {} fields + {} values + {} templates = {:.1} KB (once, protected, rebuildable)\n", d.fields.len(), d.vals.len(), d.tmpl.len(), dict_bytes as f64/1024.0);

    let enc: Vec<Vec<u8>> = parsed.iter().map(|v| { let mut o=Vec::new(); enc_record(v,&d,&mut o); o }).collect();
    for i in (0..n).step_by(97) { assert_eq!(dec_record(&enc[i],&d), parsed[i], "roundtrip mismatch @ {i}"); }
    println!("  roundtrip verified (zero data loss)\n");

    // ── LEVEL 1 (chosen): metadata-only shared state — field names ONLY.
    //    No value interning, no templates → every value byte lives in its record.
    let d1 = Dicts { field_id: d.field_id.clone(), fields: d.fields.clone(), val_id: HashMap::new(), vals: vec![], tmpl: HashMap::new() };
    let enc1: Vec<Vec<u8>> = parsed.iter().map(|v| { let mut o=Vec::new(); enc_record(v,&d1,&mut o); o }).collect();
    for i in (0..n).step_by(97) { assert_eq!(dec_record(&enc1[i],&d1), parsed[i], "L1 roundtrip mismatch @ {i}"); }
    let d1_bytes: usize = d1.fields.iter().map(|s| s.len()+2).sum();
    let c1_total = enc1.iter().map(|b| b.len()).sum::<usize>() + d1_bytes;

    let c_total = enc.iter().map(|b| b.len()).sum::<usize>() + dict_bytes;
    let z_total = recs.iter().map(|s| zstd::bulk::compress(s.as_bytes(),3).unwrap().len()).sum::<usize>();

    let mut sink=0u64;
    let seq: Vec<usize> = (0..1_000_000usize).map(|i| i.wrapping_mul(2654435761)%n).collect();
    let t=Instant::now(); for &i in &seq { let v:Value=serde_json::from_str(&recs[i]).unwrap(); sink=sink.wrapping_add(v.as_object().unwrap().len() as u64); } let raw_parse=t.elapsed().as_secs_f64()*1e9/seq.len() as f64;
    let t=Instant::now(); for &i in &seq { sink=sink.wrapping_add(dec_record(&enc1[i],&d1).as_object().unwrap().len() as u64); } let c1_read=t.elapsed().as_secs_f64()*1e9/seq.len() as f64;
    let want1=d1.field_id["status"];
    let t=Instant::now(); for &i in &seq { sink=sink.wrapping_add(get_field(&enc1[i],want1,&d1).and_then(|v|v.as_str().map(|s|s.len())).unwrap_or(0) as u64); } let c1_field=t.elapsed().as_secs_f64()*1e9/seq.len() as f64;
    let t=Instant::now(); for &i in &seq { let v:Value=serde_json::from_str(&recs[i]).unwrap(); sink=sink.wrapping_add(v.get("status").and_then(|s|s.as_str()).map(|s|s.len()).unwrap_or(0) as u64); } let raw_field=t.elapsed().as_secs_f64()*1e9/seq.len() as f64;
    std::hint::black_box(sink);

    println!("  A. raw                              {:>6.1} MB   1.00x", mb(raw_total));
    println!("  B. per-record zstd                  {:>6.1} MB   {:.2}x", mb(z_total), raw_total as f64/z_total as f64);
    println!("  C1. metadata-only (CHOSEN)          {:>6.1} MB   {:.2}x   shared state = field NAMES only", mb(c1_total), raw_total as f64/c1_total as f64);
    println!("  C3. +interning+templates (rejected) {:>6.1} MB   {:.2}x   (puts value data in shared state)", mb(c_total), raw_total as f64/c_total as f64);
    println!("\n  C1 read -> full Value:  raw parse {raw_parse:.0} ns   |   C1 decode {c1_read:.0} ns  ({:.2}x)", c1_read/raw_parse);
    println!("  C1 read -> one field:   raw parse {raw_field:.0} ns   |   C1 skip-scan {c1_field:.0} ns  ({:.2}x)", c1_field/raw_field);
    println!("\n  C1: no user data in shared state — losing the field table loses column NAMES");
    println!("      (recoverable from CREATE TABLE), never a byte of actual data.");
}
