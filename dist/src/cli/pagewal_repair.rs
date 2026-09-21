fn main()->Result<(),Box<dyn std::error::Error>>{
    let a:Vec<String>=std::env::args().collect();
    if a.len()!=4{return Err("SOURCE NEW_DESTINATION MAX_VALUE_BYTES".into());}
    let report=sekejap_core::pagewal::recover_to(std::path::Path::new(&a[1]),std::path::Path::new(&a[2]),a[3].parse()?)?;
    println!("{}",serde_json::to_string_pretty(&report)?);Ok(())
}
