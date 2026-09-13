use e4_prototype::*;
use serde_json::json;

fn layout() -> Layout {
    Layout {
        id: 7,
        fields: vec![
            ("name".into(), Kind::Text),
            ("born".into(), Kind::Int),
            ("profile".into(), Kind::Json),
            ("location".into(), Kind::Geo),
            ("embedding".into(), Kind::Vector(3)),
            ("optional".into(), Kind::Text),
        ],
    }
}
fn roundtrip(doc: serde_json::Value) {
    let l = layout();
    let e = encode(&l, &doc).unwrap();
    let got = decode(&l, &e.row, |field| {
        Ok(e.vectors
            .iter()
            .find(|(f, _)| *f == field)
            .unwrap()
            .1
            .clone())
    })
    .unwrap();
    assert_eq!(got, doc);
}

#[test]
fn undeclared_nested_fields_roundtrip_first() {
    roundtrip(
        json!({"name":"Maya", "born":19490622, "profile":{"languages":["id","en"],"preferences":{"quiet":true}}, "source":"survey", "unknown":{"array":[null,true,-128,1.25,{"x":"é\u{0000}"}]}}),
    );
}
#[test]
fn missing_null_and_json_null_are_not_empty_objects() {
    for doc in [
        json!({}),
        json!({"optional":null}),
        json!({"profile":null}),
        json!({"profile":{}}),
        json!({"profile":[null]}),
        json!({"source":null}),
    ] {
        roundtrip(doc);
    }
}
#[test]
fn integers_keep_all_bits_and_float_values() {
    for n in [
        i64::MIN,
        -32769,
        -32768,
        -129,
        -128,
        -1,
        0,
        1,
        127,
        128,
        255,
        32767,
        32768,
        i64::MAX,
    ] {
        roundtrip(json!({"born":n,"other":n}));
    }
    roundtrip(json!({"other":u64::MAX,"profile":[1.25,1e100,-0.0]}));
}
#[test]
fn vector_is_external_and_point_binary() {
    let doc = json!({"location":{"type":"Point","coordinates":[144.96,-37.81]},"embedding":[0.25,0.5,0.75]});
    let e = encode(&layout(), &doc).unwrap();
    assert_eq!(e.vectors[0].1.len(), 12);
    assert!(e.row.len() < 32);
    roundtrip(doc);
}
#[test]
fn every_truncated_record_is_rejected() {
    let l = layout();
    let e = encode(
        &l,
        &json!({"name":"Maya","profile":{"a":[1,2,3]},"extra":"hi"}),
    )
    .unwrap();
    for end in 0..e.row.len() {
        assert!(
            decode(&l, &e.row[..end], |_| Err("unexpected vector".into())).is_err(),
            "end={end}"
        );
    }
    let mut trailing = e.row;
    trailing.push(0);
    assert!(decode(&l, &trailing, |_| Err("unexpected".into())).is_err());
}
#[test]
fn wrong_layout_and_wrong_types_are_rejected() {
    assert!(encode(&layout(), &json!({"born":"123"})).is_err());
    assert!(encode(&layout(), &json!({"embedding":[1,2]})).is_err());
    let e = encode(&layout(), &json!({})).unwrap();
    let mut changed = layout();
    changed.id += 1;
    assert!(decode(&changed, &e.row, |_| Err("unexpected".into())).is_err());
}

#[test]
fn descriptors_detect_each_single_byte_change() {
    let l = layout();
    let b = l.descriptor().unwrap();
    assert_eq!(Layout::from_descriptor(&b).unwrap(), l);
    for i in 0..b.len() {
        let mut bad = b.clone();
        bad[i] ^= 1;
        assert!(Layout::from_descriptor(&bad).is_err(), "byte {i}");
    }
}
#[test]
fn layout_versions_and_general_geometry_roundtrip() {
    let old = layout();
    let mut new = old.clone();
    new.id = 8;
    new.fields.push(("added".into(), Kind::Bool));
    let d = json!({"name":"Maya","added":true,"location":{"type":"Polygon","coordinates":[[[144.0,-38.0],[145.0,-38.0],[145.0,-37.0],[144.0,-38.0]]]}});
    for l in [old, new] {
        let e = encode(&l, &d).unwrap();
        assert_eq!(
            decode(&l, &e.row, |_| Err("unexpected vector".into())).unwrap(),
            d
        );
    }
}
#[test]
fn randomized_optional_and_dynamic_shapes() {
    for i in 0..1000 {
        let mut d = json!({"born":i*65537-128,"profile":{"a":[i, i%2==0,null,{"unicode":"雪🦀"}]}});
        if i % 2 == 0 {
            d["name"] = json!(format!("row-{i}"));
        }
        if i % 3 == 0 {
            d["optional"] = Value::Null;
        }
        if i % 5 == 0 {
            d[format!("dynamic-{i}")] = json!([1.25, "extra"]);
        }
        roundtrip(d);
    }
}
#[test]
fn corrupted_lengths_and_tags_never_panic() {
    let l = layout();
    let e = encode(
        &l,
        &json!({"name":"text","profile":{"nested":[1,true,null]},"extra":"x"}),
    )
    .unwrap();
    for i in 0..e.row.len() {
        for mask in [1, 127, 255] {
            let mut bad = e.row.clone();
            bad[i] ^= mask;
            let _ = decode(&l, &bad, |_| Err("unexpected vector".into()));
        }
    }
}

#[test]
fn json_wire_preserves_promoted_fp32_coordinates() {
    let expected = (-0.09940725_f32) as f64;
    let wire = serde_json::to_string(&expected).unwrap();
    let parsed: Value = serde_json::from_str(&wire).unwrap();
    assert_eq!(parsed.as_f64().unwrap().to_bits(), expected.to_bits());
}

#[test]
fn projection_matches_document_paths_including_missing_and_null() {
    let l = layout();
    for p in [
        json!({"nested":{"quiet":true},"long":"x".repeat(10000)}),
        json!({"nested":{"quiet":null}}),
        json!({"nested":{}}),
        json!([1, 2]),
    ] {
        let d = json!({"born":19490622,"profile":p,"name":"Maya"});
        let e = encode(&l, &d).unwrap();
        let got = project(
            &l,
            &e.row,
            &[("born", &[]), ("profile", &["nested", "quiet"])],
        )
        .unwrap();
        assert_eq!(got[0], Some(d["born"].clone()));
        assert_eq!(
            got[1],
            d["profile"]
                .get("nested")
                .and_then(|v| v.get("quiet"))
                .cloned()
        );
        for end in 0..e.row.len() {
            assert!(project(&l, &e.row[..end], &[("profile", &["nested", "quiet"])]).is_err());
        }
    }
}

use serde_json::Value;

#[test]
fn dense_header_preserves_optional_fields_and_point_contract() {
    let l = Layout {
        id: 8,
        fields: vec![
            ("a".into(), Kind::Int),
            ("b".into(), Kind::Text),
            ("location".into(), Kind::Point),
        ],
    };
    for d in [
        json!({}),
        json!({"a":null}),
        json!({"a":19490622,"b":"text","location":{"type":"Point","coordinates":[144.0,-38.0]}}),
        json!({"b":"text","extra":{"a":[null,1]}}),
    ] {
        let e = encode_dense(&l, &d).unwrap();
        assert_eq!(
            decode_dense(&l, &e.row, |_| Err("vector".into())).unwrap(),
            d
        );
        for n in 0..e.row.len() {
            assert!(decode_dense(&l, &e.row[..n], |_| Err("vector".into())).is_err());
        }
    }
    assert!(encode_dense(
        &l,
        &json!({"location":{"type":"LineString","coordinates":[[1,2],[3,4]]}})
    )
    .is_err());
}

#[test]
fn packed_integer_widths_preserve_full_range_missing_null_and_extras() {
    let l = Layout {
        id: 31,
        fields: (0..12)
            .map(|i| (format!("i{i}"), Kind::Int))
            .chain([("tail".into(), Kind::Json)])
            .collect(),
    };
    for offset in 0..12 {
        let mut doc = serde_json::Map::new();
        for (i, v) in [
            i64::MIN,
            -65537,
            -129,
            -128,
            0,
            1,
            127,
            128,
            32767,
            32768,
            1 << 48,
            i64::MAX,
        ]
        .into_iter()
        .enumerate()
        {
            if i == offset {
                continue;
            }
            doc.insert(
                format!("i{i}"),
                if i == (offset + 1) % 12 {
                    Value::Null
                } else {
                    json!(v)
                },
            );
        }
        doc.insert("tail".into(), json!({"nested":[null,true,1.25]}));
        doc.insert("unknown".into(), json!(u64::MAX));
        let doc = Value::Object(doc);
        let e = encode_dense_v3(&l, &doc).unwrap();
        assert_eq!(
            decode_dense_v3(&l, &e.row, |_| Err("vector".into())).unwrap(),
            doc
        );
        for end in 0..e.row.len() {
            assert!(decode_dense_v3(&l, &e.row[..end], |_| Err("vector".into())).is_err());
        }
    }
}
