//! Golden corpus for the redaction layer (`tests/redact_corpus.txt`).

use kevin_telemetry::Redactor;

#[test]
fn redact_corpus_matches_golden_outputs() {
    let corpus = include_str!("redact_corpus.txt");
    let redactor = Redactor::default();
    let mut pending: Option<String> = None;
    let mut checked = 0;
    for (n, line) in corpus.lines().enumerate() {
        let line_no = n + 1;
        if line.trim().is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(input) = line.strip_prefix("in: ") {
            assert!(pending.is_none(), "line {line_no}: `in:` without `out:`");
            pending = Some(input.replace("\\n", "\n"));
        } else if let Some(expected) = line.strip_prefix("out: ") {
            let input = pending
                .take()
                .unwrap_or_else(|| panic!("line {line_no}: `out:` without `in:`"));
            let expected = expected.replace("\\n", "\n");
            let actual = redactor.redact_str(&input);
            assert_eq!(actual, expected, "corpus line {line_no}: input `{input}`");
            checked += 1;
        } else {
            panic!("line {line_no}: unexpected `{line}`");
        }
    }
    assert!(pending.is_none(), "trailing `in:` without `out:`");
    assert!(checked >= 15, "corpus too small: {checked}");
}
