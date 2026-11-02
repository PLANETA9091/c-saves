//! Probe: what framing does fastnbt::from_bytes expect for a root value?
//! Goal: parse a bare TAG_List payload (sections sub-slice) by wrapping it
//! as [tag][name][payload] without a full-tree parse.

use fastnbt::Value;

fn main() {
    // build a list value and serialize it named-root style via our wrap()
    let list = Value::List(vec![
        Value::Compound(
            [("Y".to_string(), Value::Byte(5)), ("Name".to_string(), Value::String("x".into()))]
                .into_iter()
                .collect(),
        ),
        Value::Int(42),
    ]);
    // named compound {"": list}
    let mut m = std::collections::HashMap::new();
    m.insert("".to_string(), list.clone());
    let named = fastnbt::to_bytes(&Value::Compound(m)).unwrap();
    println!("named compound bytes: {:02x?}", &named[..named.len().min(24)]);

    // parse it back (this is what join() does with tails)
    let v: Value = fastnbt::from_bytes(&named).unwrap();
    println!("named roundtrip ok: {v:?}");

    // Now: bare [tag][name][payload] for a LIST root
    let mut bare = Vec::new();
    bare.push(9u8); // TAG_List
    bare.extend_from_slice(&0u16.to_be_bytes()); // empty name
    // list payload: element type + len + elements — serialize elements by hand:
    // element 0: TAG_Compound, element 1: TAG_Int -> mixed types NOT allowed in NBT lists!
    // redo: uniform list of compounds
    let list2 = Value::List(vec![Value::Compound(
        [("Y".to_string(), Value::Byte(5)), ("Name".to_string(), Value::String("x".into()))]
            .into_iter()
            .collect(),
    )]);
    let mut m2 = std::collections::HashMap::new();
    m2.insert("".to_string(), list2.clone());
    let named2 = fastnbt::to_bytes(&Value::Compound(m2)).unwrap();
    // named2 = [0x0A][00 00][0x09][00 00][list payload][0x00]
    // list payload = named2[5..named2.len()-1]
    let lp = &named2[5..named2.len() - 1];
    let mut bare2 = Vec::new();
    bare2.push(9u8);
    bare2.extend_from_slice(&0u16.to_be_bytes());
    bare2.extend_from_slice(lp);
    let v2: Result<Value, _> = fastnbt::from_bytes(&bare2);
    match v2 {
        Ok(v) => println!("bare list parse OK: {:?}", v),
        Err(e) => println!("bare list parse FAIL: {e}"),
    }

    // does from_bytes skip the name for the root? try [tag][payload] (no name)
    let mut noname = Vec::new();
    noname.push(9u8);
    noname.extend_from_slice(lp);
    let v3: Result<Value, _> = fastnbt::from_bytes(&noname);
    match v3 {
        Ok(v) => println!("noname list parse OK: {:?}", v),
        Err(e) => println!("noname list parse FAIL: {e}"),
    }
}
