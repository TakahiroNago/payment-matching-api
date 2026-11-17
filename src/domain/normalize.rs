//! Normalization of payer names (振込依頼人名).
//!
//! Bank statements carry the payer in half-width katakana with abbreviated legal forms (`ｶ)ﾔﾏﾀﾞｼﾖｳｼﾞ`), while a
//! customer master usually holds the reading as typed by a person (`カブシキガイシャ ヤマダショウジ`). Both are
//! reduced to the same key here. The key is only compared for equality: there is no fuzzy matching, because a
//! wrong match costs more than a payment left for a person to look at.
//!
//! The normalization is not complete. Names with a branch or a person appended, and legal forms missing from
//! the tables below, are handled by registering an alias.

use unicode_normalization::UnicodeNormalization;

/// Abbreviations of legal forms as they appear with parentheses: `カ)…`, `…(カ` and `(カ)`.
const ENTITY_ABBREVIATIONS: &[&str] = &[
    "カ",
    "ユ",
    "ド",
    "メ",
    "シ",
    "ザイ",
    "シヤ",
    "イ",
    "ガク",
    "フク",
    "シユウ",
    "株",
    "有",
    "同",
    "名",
    "資",
    "財",
    "社",
    "医",
    "学",
    "福",
    "宗",
];

/// Legal forms written out, removed at the start and at the end of the name.
const ENTITY_WORDS: &[&str] = &[
    "カブシキガイシヤ",
    "ユウゲンガイシヤ",
    "ゴウドウガイシヤ",
    "ゴウメイガイシヤ",
    "ゴウシガイシヤ",
    "株式会社",
    "有限会社",
    "合同会社",
    "合名会社",
    "合資会社",
];

/// Returns the comparison key for a payer name, or `None` when nothing is left to compare.
///
/// 1. NFKC: half-width katakana to full-width (voiced marks composed), full-width letters and digits to ASCII.
/// 2. Whitespace removed; hiragana to katakana; small kana to large (banks do not use small kana); ASCII
///    letters to upper case.
/// 3. Abbreviated legal forms removed (`カ)`, `(カ`, `(カ)`).
/// 4. Separators removed, including the long vowel mark and the hyphen that often stands in for it.
/// 5. Legal forms written out removed from both ends.
///
/// A key is a fixed point: normalizing a key returns the same key. Removing characters can bring a letter and
/// a combining mark together, or uncover another legal form, so the steps are repeated until nothing changes.
#[must_use]
pub fn normalize_payer_name(raw: &str) -> Option<String> {
    let mut key = normalize_once(raw);
    // Every pass after the first removes characters or stops; the bound only guards against a surprise.
    for _ in 0..8 {
        let next = normalize_once(&key);
        if next == key {
            break;
        }
        key = next;
    }
    if key.is_empty() { None } else { Some(key) }
}

fn normalize_once(raw: &str) -> String {
    let folded: String = raw
        .nfkc()
        .filter(|c| !c.is_whitespace())
        .map(fold_char)
        .collect();
    let without_abbreviation = strip_abbreviated_entity(&folded);
    let without_separators: String = without_abbreviation
        .chars()
        .filter(|c| !is_separator(*c))
        .collect();
    strip_entity_words(&without_separators).to_owned()
}

fn fold_char(c: char) -> char {
    // Hiragana (ぁ..ゖ) to katakana: the two blocks are laid out in parallel, 0x60 apart.
    let c = match c {
        '\u{3041}'..='\u{3096}' => char::from_u32(c as u32 + 0x60).unwrap_or(c),
        _ => c,
    };
    match c {
        'ァ' => 'ア',
        'ィ' => 'イ',
        'ゥ' => 'ウ',
        'ェ' => 'エ',
        'ォ' => 'オ',
        'ッ' => 'ツ',
        'ャ' => 'ヤ',
        'ュ' => 'ユ',
        'ョ' => 'ヨ',
        'ヮ' => 'ワ',
        'ヵ' => 'カ',
        'ヶ' => 'ケ',
        _ => c.to_ascii_uppercase(),
    }
}

fn is_separator(c: char) -> bool {
    matches!(
        c,
        '(' | ')' | '・' | '.' | ',' | '/' | '\\' | '¥' | '「' | '」' | '-' | 'ー' | '\u{2010}'
            ..='\u{2015}' | '\u{2212}'
    )
}

fn strip_abbreviated_entity(name: &str) -> String {
    let mut s = name.to_owned();
    for abbreviation in ENTITY_ABBREVIATIONS {
        s = s.replace(&format!("({abbreviation})"), "");
    }
    for abbreviation in ENTITY_ABBREVIATIONS {
        if let Some(rest) = s.strip_prefix(&format!("{abbreviation})")) {
            s = rest.to_owned();
            break;
        }
    }
    for abbreviation in ENTITY_ABBREVIATIONS {
        if let Some(rest) = s.strip_suffix(&format!("({abbreviation}")) {
            s = rest.to_owned();
            break;
        }
    }
    s
}

fn strip_entity_words(name: &str) -> &str {
    let mut s = name;
    for word in ENTITY_WORDS {
        if let Some(rest) = s.strip_prefix(word) {
            s = rest;
            break;
        }
    }
    for word in ENTITY_WORDS {
        if let Some(rest) = s.strip_suffix(word) {
            s = rest;
            break;
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::normalize_payer_name;

    #[test]
    fn known_cases() {
        let cases: &[(&str, Option<&str>)] = &[
            // Half-width katakana, as in a bank statement.
            ("ｶ)ﾔﾏﾀﾞｼﾖｳｼﾞ", Some("ヤマダシヨウジ")),
            ("ﾔﾏﾀﾞｼﾖｳｼﾞ(ｶ", Some("ヤマダシヨウジ")),
            ("ﾔﾏﾀﾞｼﾖｳｼﾞ(ｶ)", Some("ヤマダシヨウジ")),
            ("ﾕ)ｻﾄｳｺｳｷﾞﾖｳ", Some("サトウコウギヨウ")),
            ("ﾄﾞ)ｱｵｿﾞﾗ", Some("アオゾラ")),
            ("ｻﾞｲ)ｵｶﾔﾏﾌﾞﾝｶ", Some("オカヤマブンカ")),
            ("ｼﾔ)ｵｶﾔﾏｹﾝﾌﾞﾝｶﾚﾝﾒｲ", Some("オカヤマケンブンカレンメイ")),
            ("ｲ)ｹﾝｺｳｶｲ", Some("ケンコウカイ")),
            ("ｶﾞｸ)ｾｲﾜｶﾞｸｴﾝ", Some("セイワガクエン")),
            ("ﾊﾟﾝﾔ ﾀﾅｶ", Some("パンヤタナカ")),
            // Full-width katakana typed by a person, with small kana and the legal form written out.
            ("カブシキガイシャ ヤマダショウジ", Some("ヤマダシヨウジ")),
            ("ヤマダショウジ カブシキガイシャ", Some("ヤマダシヨウジ")),
            ("ユウゲンガイシャサトウコウギョウ", Some("サトウコウギヨウ")),
            ("ゴウドウガイシャ アオゾラ", Some("アオゾラ")),
            ("カ）ヤマダシヨウジ", Some("ヤマダシヨウジ")),
            ("（カ）ヤマダシヨウジ", Some("ヤマダシヨウジ")),
            // Hiragana.
            ("やまだしょうじ", Some("ヤマダシヨウジ")),
            ("かぶしきがいしゃ やまだしょうじ", Some("ヤマダシヨウジ")),
            // Kanji forms and the compatibility character for (株).
            ("株式会社山田商事", Some("山田商事")),
            ("山田商事株式会社", Some("山田商事")),
            ("㈱山田商事", Some("山田商事")),
            ("山田商事（株）", Some("山田商事")),
            ("(有)佐藤工業", Some("佐藤工業")),
            // Letters, digits and separators.
            ("ｶ)ABC ｼﾖｳｼﾞ", Some("ABCシヨウジ")),
            ("ａｂｃ　ｓｈｏｊｉ", Some("ABCSHOJI")),
            ("Abc-Shoji", Some("ABCSHOJI")),
            ("ｴｰ･ﾋﾞｰ･ｼｰ", Some("エビシ")),
            ("ｺ-ﾎﾟﾚ-ｼﾖﾝ ﾀﾅｶ", Some("コポレシヨンタナカ")),
            ("コーポレーション タナカ", Some("コポレシヨンタナカ")),
            ("ﾀﾅｶ.ﾀﾛｳ", Some("タナカタロウ")),
            ("ﾀﾅｶ ﾀﾛｳ", Some("タナカタロウ")),
            ("１２３ｼﾖｳｶｲ", Some("123シヨウカイ")),
            // Names that merely contain the letters of an abbreviation are left alone.
            ("カトウシヨウテン", Some("カトウシヨウテン")),
            ("ユニオンカ", Some("ユニオンカ")),
            // Nothing left to compare.
            ("", None),
            ("   ", None),
            ("ｶ)", None),
            ("株式会社", None),
            ("-・-", None),
        ];
        for (raw, expected) in cases {
            assert_eq!(
                normalize_payer_name(raw).as_deref(),
                *expected,
                "input: {raw:?}"
            );
        }
    }

    #[test]
    fn statement_and_master_forms_agree() {
        let statement = normalize_payer_name("ｶ)ﾔﾏﾀﾞｼﾖｳｼﾞ");
        let master = normalize_payer_name("カブシキガイシャ　ヤマダショウジ");
        assert!(statement.is_some());
        assert_eq!(statement, master);
    }

    proptest! {
        /// Keys are stored and compared as they are, so a key must normalize to itself.
        #[test]
        fn normalization_is_idempotent_on_name_like_input(
            raw in "[ｦ-ﾟァ-ヶぁ-んA-Za-zＡ-Ｚ0-9０-９ 　()（）㈱株式会社有限・.ー-]{0,24}"
        ) {
            if let Some(key) = normalize_payer_name(&raw) {
                prop_assert_eq!(normalize_payer_name(&key), Some(key));
            }
        }

        #[test]
        fn normalization_is_idempotent_on_any_input(raw in "\\PC{0,24}") {
            if let Some(key) = normalize_payer_name(&raw) {
                prop_assert_eq!(normalize_payer_name(&key), Some(key));
            }
        }

        #[test]
        fn key_has_no_whitespace_or_separators(raw in "\\PC{0,24}") {
            if let Some(key) = normalize_payer_name(&raw) {
                prop_assert!(!key.is_empty());
                prop_assert!(
                    !key.chars().any(|c| c.is_whitespace() || matches!(c, '(' | ')' | '-' | 'ー' | '・'))
                );
            }
        }
    }
}
