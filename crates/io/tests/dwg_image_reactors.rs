//! AutoCAD's External References palette lists an image as unreferenced when its IMAGEDEF lists
//! no reactor, though it draws it. CADCraft's DXF lists them (the image dictionary, then one
//! IMAGEDEF_REACTOR per image); the DWG bridge must keep them, and RASTERVARIABLES' too.
#![cfg(not(target_arch = "wasm32"))]

use std::io::Cursor;

use cadcraft_doc::{Common, Drawing, EntityKind, Image, Space};
use cadcraft_geom::{Vec2, Vec3};
use cadcraft_io::{SaveOptions, Version, write_with};

/// Two images of one picture.
fn pictures() -> Drawing {
    let mut d = Drawing::new_metric();
    for x in [0.0, 20.0] {
        let image = Image {
            insert: Vec3::new(x, 0.0, 0.0),
            u: Vec3::new(0.1, 0.0, 0.0),
            v: Vec3::new(0.0, 0.1, 0.0),
            size: Vec2::new(100.0, 50.0),
            path: "/pictures/picture.png".into(),
            name: "picture".into(),
            clip: Vec::new(),
            clipping: false,
            display: 15,
            brightness: 50,
            contrast: 50,
            fade: 0,
            pixel_size: Vec2::new(0.01, 0.01),
            resolution_units: 0,
        };
        d.add(&Space::Model, Common::default(), EntityKind::Image(image)).expect("add the image");
    }
    d
}

/// The `{ACAD_REACTORS` handles of the first object of type `kind` in DXF `text`.
fn reactors(text: &str, kind: &str) -> Vec<String> {
    let lines: Vec<&str> = text.lines().map(str::trim).collect();
    let pairs: Vec<(&str, &str)> = lines.chunks(2).map(|p| (p[0], p.get(1).copied().unwrap_or(""))).collect();
    let start = pairs.iter().position(|p| *p == ("0", kind)).unwrap_or_else(|| panic!("no {kind}"));
    let mut out = Vec::new();
    let mut inside = false;
    for &(code, value) in &pairs[start + 1..] {
        match (code, value) {
            ("0", _) => break,
            ("102", "{ACAD_REACTORS") => inside = true,
            ("102", "}") => inside = false,
            ("330", h) if inside => out.push(h.to_string()),
            _ => {}
        }
    }
    out
}

#[test]
fn a_dwg_keeps_the_reactors_of_its_image_definitions() {
    let d = pictures();
    let dwg = write_with(&d, "pictures.dwg", &SaveOptions { version: Some(Version::R2018), binary: false }).expect("write the DWG");
    // Read back by acadrust, and its objects written out as DXF to see their reactors.
    let doc = acadrust::DwgReader::from_stream(Cursor::new(dwg)).read().expect("read the DWG back");
    let text = String::from_utf8(acadrust::DxfWriter::new(&doc).write_to_vec().expect("write DXF")).expect("ASCII DXF");
    let hex = |h: acadrust::Handle| format!("{:X}", h.value());
    let definition = doc
        .objects
        .values()
        .find_map(|o| if let acadrust::objects::ObjectType::ImageDefinition(d) = o { Some(d) } else { None })
        .expect("an IMAGEDEF");
    let mut want = vec![hex(definition.owner)];
    want.extend(doc.entities().filter_map(|e| match e {
        acadrust::entities::EntityType::RasterImage(i) => i.definition_reactor_handle.map(hex),
        _ => None,
    }));
    assert_eq!(want.len(), 3, "the image dictionary and two reactors");
    assert_eq!(reactors(&text, "IMAGEDEF"), want);
    assert_eq!(reactors(&text, "RASTERVARIABLES").len(), 1, "the named object dictionary");
}
