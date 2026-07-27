//! Store ONE record in Level-1 SKBIN (no FSST, strings literal) and print its
//! exact bytes on disk, annotated tag-by-tag.
//!
//!   cargo bench --bench dump_record

use sekejap::{Config, CoreDB};

fn get_uv(b: &[u8], p: &mut usize) -> u64 {
    let mut v = 0u64; let mut s = 0;
    loop { let x = b[*p]; *p += 1; v |= ((x & 0x7f) as u64) << s; if x & 0x80 == 0 { break } s += 7; }
    v
}
fn unzig(v: u64) -> i64 { ((v >> 1) as i64) ^ -((v & 1) as i64) }
fn hex(b: &[u8]) -> String { b.iter().map(|x| format!("{x:02x} ")).collect() }

fn main() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = Config { payload_binary: true, ..Config::default() }; // Level-1, NO fsst
    {
        let mut db = CoreDB::open_with_config(dir.path(), cfg).unwrap();
        db.put("t/a1", r#"{"_collection":"t","_key":"a1","status":"shipped","qty":42}"#).unwrap();
        db.compact().unwrap();
    }

    // field table: id → name
    let ft = std::fs::read(dir.path().join("field_table.bin")).unwrap();
    let mut names = Vec::new();
    { let p0=&ft[9..]; let mut p=0; let n=get_uv(p0,&mut p) as usize;
      for _ in 0..n { let l=get_uv(p0,&mut p) as usize; names.push(std::str::from_utf8(&p0[p..p+l]).unwrap().to_string()); p+=l; } }

    // the one record = the whole payloads.bin
    let r = std::fs::read(dir.path().join("payloads.bin")).unwrap();
    println!("Level-1 record on disk — {} bytes total\n", r.len());
    println!("  {}   frame tag: 0x02 = 'this is a SKBIN record'", hex(&r[0..1]));
    println!("  {}   CRC32 of the body (corruption check)", hex(&r[1..5]));

    let mut p = 5;
    let objtag = r[p]; p += 1;
    println!("  {}   0x08 = object", hex(&[objtag]));
    let nfields = get_uv(&r, &mut p);
    println!("  ..   {nfields} fields follow, each = [field-id][value]\n");

    for _ in 0..nfields {
        let start = p;
        let fid = get_uv(&r, &mut p) as usize;
        let name = &names[fid];
        let tag = r[p]; p += 1;
        let (desc, valbytes_end);
        match tag {
            0 => { desc = "null".to_string(); valbytes_end = p; }
            1 => { desc = "false".to_string(); valbytes_end = p; }
            2 => { desc = "true".to_string(); valbytes_end = p; }
            3 => { let v = unzig(get_uv(&r, &mut p)); desc = format!("int {v}"); valbytes_end = p; }
            4 => { p += 8; desc = "float(8 bytes)".to_string(); valbytes_end = p; }
            5 => { let v = get_uv(&r, &mut p); desc = format!("uint {v}"); valbytes_end = p; }
            6 => { let l = get_uv(&r, &mut p) as usize; let s = std::str::from_utf8(&r[p..p+l]).unwrap().to_string(); p += l; desc = format!("string {s:?} ({l} bytes, LITERAL)"); valbytes_end = p; }
            _ => { desc = format!("tag {tag}"); valbytes_end = p; }
        }
        println!("  {:<32}  field {fid} = {name:?}", hex(&r[start..valbytes_end]));
        println!("  {:<32}  → value: {desc}\n", "");
    }
}
