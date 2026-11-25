use super::*;

fn contains(range: &CharRange, c: u32) -> bool {
    range.points.chunks_exact(2).any(|p| p[0] <= c && c < p[1])
}

fn conversion(c: u32, conv: CaseConv) -> Vec<u32> {
    let (output, len) = case_conv(c, conv);
    output[..len].to_vec()
}

#[test]
fn case_conversion_maps_full_sequences() {
    use CaseConv::{CaseFold, Lower, Upper};
    let cases: &[(u32, CaseConv, &[u32])] = &[
        (0x61, Upper, &[0x41]),
        (0x41, Lower, &[0x61]),
        (0x41, CaseFold, &[0x61]),
        (0xdf, Upper, &[0x53, 0x53]),
        (0x130, Lower, &[0x69, 0x307]),
        (0x130, CaseFold, &[0x69, 0x307]),
        (0x1c5, Upper, &[0x1c4]),
        (0x1c5, Lower, &[0x1c6]),
        (0x3c2, Upper, &[0x3a3]),
        (0x3c2, Lower, &[0x3c2]),
        (0x3c2, CaseFold, &[0x3c3]),
        (0x390, Upper, &[0x399, 0x308, 0x301]),
        (0xfb03, CaseFold, &[0x66, 0x66, 0x69]),
        (0x10400, Lower, &[0x10428]),
        (0xdfff, CaseFold, &[0xdfff]),
        (0x10ffff, Upper, &[0x10ffff]),
    ];
    assert_eq!(CASE_CONV_MAX_LEN, 3);
    for &(c, conv, expected) in cases {
        assert_eq!(conversion(c, conv), expected, "{c:x} {conv:?}");
    }
}

#[test]
fn regexp_case_folding_uses_single_code_points() {
    let cases = [
        (0x61, 0x41, 0x61),
        (0xdf, 0xdf, 0xdf),
        (0x17f, 0x17f, 0x73),
        (0x212a, 0x212a, 0x6b),
        (0x1e9e, 0x1e9e, 0xdf),
        (0xfb06, 0xfb06, 0xfb05),
        (0x1fd3, 0x1fd3, 0x390),
        (0x1fe3, 0x1fe3, 0x3b0),
    ];
    for (c, legacy, unicode) in cases {
        assert_eq!(canonicalize(c, false), legacy, "{c:x}");
        assert_eq!(canonicalize(c, true), unicode, "{c:x}");
    }
}

#[test]
fn cased_and_case_ignorable_are_independent() {
    assert!(is_cased(0x41));
    assert!(is_cased(0x2b0));
    assert!(is_case_ignorable(0x2b0));
    assert!(is_case_ignorable(0x301));
    assert!(!is_cased(0x301));
    assert!(!is_cased(0xd800));
    assert!(!is_case_ignorable(0x10ffff));
}

#[test]
fn javascript_identifiers_use_the_ecmascript_alphabet() {
    for c in [0x24, 0x5f] {
        assert!(is_js_ident_start(c));
        assert!(is_js_ident_continue(c));
        assert!(!is_id_start(c));
    }
    assert!(!is_id_continue(0x24));
    assert!(is_id_continue(0x5f));
    for c in [0x30, 0x301, 0x200c, 0x200d] {
        assert!(!is_js_ident_start(c));
        assert!(is_js_ident_continue(c));
    }
    for c in [0x41, 0xaa, 0x2118, 0x10400] {
        assert!(is_id_start(c));
        assert!(is_id_continue(c));
        assert!(is_js_ident_start(c));
        assert!(is_js_ident_continue(c));
    }
    for c in [0, 0x1f600, 0xd800, 0x10ffff, u32::MAX] {
        assert!(!is_js_ident_start(c));
        assert!(!is_js_ident_continue(c));
    }
}

#[test]
fn javascript_and_unicode_whitespace_differ() {
    for c in [
        9, 10, 11, 12, 13, 0xa0, 0x1680, 0x200a, 0x2028, 0x2029, 0x202f, 0x205f, 0x3000,
    ] {
        assert!(is_space(c));
        assert!(is_white_space(c));
    }
    assert!(is_space(0xfeff));
    assert!(!is_white_space(0xfeff));
    assert!(is_white_space(0x85));
    assert!(!is_space(0x85));
    for c in [0x180e, 0x200b, 0x1f600, u32::MAX] {
        assert!(!is_space(c));
        assert!(!is_white_space(c));
    }
}

#[test]
fn range_operations_merge_and_split_at_shared_endpoints() {
    use CharRangeOp::{Inter, Sub, Union, Xor};
    let a = [1, 4, 4, 6, 9, 10];
    let b = [0, 2, 3, 5, 6, 9, 10, 11];
    let cases: &[(CharRangeOp, &[u32])] = &[
        (Union, &[0, 11]),
        (Inter, &[1, 2, 3, 5]),
        (Xor, &[0, 1, 2, 3, 5, 11]),
        (Sub, &[2, 3, 5, 6, 9, 10]),
    ];
    for &(op, expected) in cases {
        let mut out = CharRange::new();
        cr_op(&mut out, &a, &b, op);
        assert_eq!(out.points, expected, "{op:?}");
        let mut inplace = CharRange { points: a.to_vec() };
        inplace.op_in_place(&b, op);
        assert_eq!(inplace, out, "{op:?}");
    }
    let mut out = CharRange { points: vec![0, 1] };
    cr_op(&mut out, &[1, 5], &[5, 9], Union);
    assert_eq!(out.points, [0, 9]);
    out.add_interval(10, 11);
    out.union_interval(9, 9);
    assert_eq!(out.points, [0, 11]);
}

#[test]
fn range_complement_uses_the_full_endpoint_domain() {
    let mut cr = CharRange::new();
    cr.invert();
    assert_eq!(cr.points, [0, u32::MAX]);
    cr.invert();
    assert!(cr.is_empty());
    cr.add_interval(0, 4);
    cr.add_interval(5, 8);
    cr.add_interval(11, u32::MAX);
    cr.invert();
    assert_eq!(cr.points, [4, 5, 8, 11]);
}

#[test]
fn regexp_canonicalization_takes_the_image_of_a_range() {
    let base = CharRange {
        points: vec![0x41, 0x5b, 0x61, 0x7b, 0x17f, 0x180, 0x212a, 0x212b],
    };
    let mut unicode = base.clone();
    unicode.regexp_canonicalize(true);
    assert_eq!(unicode.points, [0x61, 0x7b]);
    let mut legacy = base;
    legacy.regexp_canonicalize(false);
    assert_eq!(legacy.points, [0x41, 0x5b, 0x17f, 0x180, 0x212a, 0x212b]);
}

#[test]
fn scripts_honor_aliases_and_extensions() {
    for is_ext in [false, true] {
        let mut short = CharRange::new();
        let mut long = CharRange::new();
        unicode_script(&mut short, "Hira", is_ext).unwrap();
        unicode_script(&mut long, "Hiragana", is_ext).unwrap();
        assert_eq!(short, long);
        assert!(contains(&long, 0x3042));
        assert_eq!(contains(&long, 0x30fc), is_ext);
        long.clear();
        unicode_script(&mut long, "Zyyy", is_ext).unwrap();
        assert_eq!(contains(&long, 0x30fc), !is_ext);
        assert!(contains(&long, 0x20));
        long.clear();
        unicode_script(&mut long, "Zzzz", is_ext).unwrap();
        assert!(!contains(&long, 0x41));
        assert!(contains(&long, 0x378));
        assert!(contains(&long, 0xd800));
        assert_eq!(long.points.last(), Some(&u32::MAX));
    }
}

#[test]
fn category_aggregates_include_alternating_case_runs() {
    let mut range = CharRange::new();
    for (name, caps, small) in [
        ("Uppercase_Letter", true, false),
        ("Ll", false, true),
        ("LC", true, true),
    ] {
        range.clear();
        unicode_general_category(&mut range, name).unwrap();
        for (c, expected) in [
            (0x100, caps),
            (0x101, small),
            (0x102, caps),
            (0x103, small),
            (0x30, false),
        ] {
            assert_eq!(contains(&range, c), expected, "{name} {c:x}");
        }
    }
    range.clear();
    unicode_general_category(&mut range, "Other").unwrap();
    assert!(contains(&range, 0x378));
    assert!(contains(&range, 0xd800));
    assert!(contains(&range, 0xe000));
    assert!(!contains(&range, 0x1f600));
}

#[test]
fn binary_property_aliases_and_derived_sets() {
    let mut range = CharRange::new();
    unicode_prop(&mut range, "Any").unwrap();
    assert_eq!(range.points, [0, 0x110000]);
    range.clear();
    unicode_prop(&mut range, "Assigned").unwrap();
    assert!(contains(&range, 0x41));
    assert!(!contains(&range, 0x378));
    range.clear();
    unicode_prop(&mut range, "Alpha").unwrap();
    assert!(contains(&range, 0x41));
    assert!(contains(&range, 0x2160));
    assert!(!contains(&range, 0x31));
    range.clear();
    unicode_prop(&mut range, "XIDC").unwrap();
    assert!(!contains(&range, 0x24));
    assert!(contains(&range, 0x5f));
    assert!(contains(&range, 0x301));
    range.clear();
    unicode_prop(&mut range, "CWCF").unwrap();
    assert!(contains(&range, 0xdf));
    assert!(contains(&range, 0x212a));
    assert!(!contains(&range, 0x61));
}

#[test]
fn invalid_property_names_do_not_emit_output() {
    let mut range = CharRange::new();
    assert_eq!(
        unicode_script(&mut range, "latin", false),
        Err(UnicodeError::InvalidProperty)
    );
    assert_eq!(
        unicode_general_category(&mut range, "Uppercase Letter"),
        Err(UnicodeError::InvalidProperty)
    );
    for name in [
        "",
        "White_Space,space",
        "ID_Compat_Math_Start",
        "Basic_Emoji",
    ] {
        assert_eq!(
            unicode_prop(&mut range, name),
            Err(UnicodeError::InvalidProperty)
        );
    }
    assert_eq!(
        unicode_sequence_prop(
            "Emoji",
            &mut |_| panic!("invalid property emitted a sequence"),
            &mut range
        ),
        Err(UnicodeError::InvalidProperty)
    );
    assert!(range.is_empty());
}

fn sequences(name: &str) -> Vec<Vec<u32>> {
    let mut out = Vec::new();
    unicode_sequence_prop(
        name,
        &mut |seq| out.push(seq.to_vec()),
        &mut CharRange::new(),
    )
    .unwrap();
    out
}

#[test]
fn sequence_properties_expand_in_table_order() {
    let groups = [
        ("Basic_Emoji", 1400, vec![0xa9, 0xfe0f]),
        ("Emoji_Keycap_Sequence", 12, vec![0x23, 0xfe0f, 0x20e3]),
        ("RGI_Emoji_Modifier_Sequence", 670, vec![0x1f44d, 0x1f3ff]),
        ("RGI_Emoji_Flag_Sequence", 259, vec![0x1f1fa, 0x1f1f8]),
        (
            "RGI_Emoji_Tag_Sequence",
            3,
            vec![
                0x1f3f4, 0xe0067, 0xe0062, 0xe0065, 0xe006e, 0xe0067, 0xe007f,
            ],
        ),
        (
            "RGI_Emoji_ZWJ_Sequence",
            1614,
            vec![0x1f469, 0x1f3fb, 0x200d, 0x1f4bb],
        ),
    ];
    let mut all = Vec::new();
    for (name, count, example) in groups {
        let seqs = sequences(name);
        assert_eq!(seqs.len(), count, "{name}");
        assert!(seqs.contains(&example), "{name}");
        all.extend(seqs);
    }
    assert_eq!(sequences("RGI_Emoji"), all);
}

#[test]
fn normalization_distinguishes_canonical_and_compatibility_forms() {
    use UnicodeNormalization::{Nfc, Nfd, Nfkc, Nfkd};
    let src = [0x212b, 0xfb03, 0x00a0, 0x2460, 0xd800, 0x10ffff];
    assert_eq!(
        normalize(&src, Nfc).unwrap(),
        [0xc5, 0xfb03, 0x00a0, 0x2460, 0xd800, 0x10ffff]
    );
    assert_eq!(
        normalize(&src, Nfd).unwrap(),
        [0x41, 0x30a, 0xfb03, 0x00a0, 0x2460, 0xd800, 0x10ffff]
    );
    assert_eq!(
        normalize(&src, Nfkc).unwrap(),
        [0xc5, 0x66, 0x66, 0x69, 0x20, 0x31, 0xd800, 0x10ffff]
    );
    assert_eq!(
        normalize(&src, Nfkd).unwrap(),
        [0x41, 0x30a, 0x66, 0x66, 0x69, 0x20, 0x31, 0xd800, 0x10ffff]
    );
    for form in [Nfc, Nfd, Nfkc, Nfkd] {
        assert!(normalize(&[], form).unwrap().is_empty());
    }
}

#[test]
fn normalization_orders_marks_and_respects_blocking() {
    use UnicodeNormalization::{Nfc, Nfd};
    assert_eq!(
        normalize(&[0x41, 0x315, 0x300, 0x323], Nfd).unwrap(),
        [0x41, 0x323, 0x300, 0x315]
    );
    assert_eq!(
        normalize(&[0x41, 0x315, 0x300, 0x323], Nfc).unwrap(),
        [0x1ea0, 0x300, 0x315]
    );
    assert_eq!(
        normalize(&[0x301, 0x323, 0x41, 0x305, 0x301], Nfc).unwrap(),
        [0x323, 0x301, 0x41, 0x305, 0x301]
    );
    assert_eq!(normalize(&[0x344], Nfc).unwrap(), [0x308, 0x301]);
}

#[test]
fn normalization_round_trips_modern_hangul() {
    use UnicodeNormalization::{Nfc, Nfd, Nfkc, Nfkd};
    for composed in 0xac00..0xd7a4 {
        let offset = composed - 0xac00;
        let jamos = [
            0x1100 + offset / 588,
            0x1161 + (offset % 588) / 28,
            0x11a7 + offset % 28,
        ];
        let expected = &jamos[..if offset % 28 == 0 { 2 } else { 3 }];
        assert_eq!(normalize(&[composed], Nfd).unwrap(), expected);
        assert_eq!(normalize(&[composed], Nfkd).unwrap(), expected);
        assert_eq!(normalize(expected, Nfc).unwrap(), [composed]);
        assert_eq!(normalize(expected, Nfkc).unwrap(), [composed]);
    }
    assert_eq!(normalize(&[0x1100, 0x1161, 0x11a7], Nfc).unwrap(), [0xac00]);
}
