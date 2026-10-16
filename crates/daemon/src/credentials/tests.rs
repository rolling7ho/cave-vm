use super::*;

fn creds(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs
        .iter()
        .map(|(n, v)| (n.to_string(), v.to_string()))
        .collect()
}

fn temp(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("cave-creds-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// Pushes `chunks` through a redactor and returns everything it emitted.
fn redact(pairs: &[(&str, &str)], chunks: &[&str]) -> String {
    let mut r = Redactor::new(&creds(pairs));
    let mut out = Vec::new();
    for chunk in chunks {
        out.extend(r.push(chunk.as_bytes()));
    }
    out.extend(r.finish());
    String::from_utf8(out).unwrap()
}

#[test]
fn env_files_are_parsed_like_a_shell_would_read_them() {
    let vars = parse_env(
        "# keys\n\
         API_KEY=snednao_#m34nr\n\
         export TOKEN = abc  # a comment\n\
         SINGLE='a \"b\" $c'\n\
         DOUBLE=\"line\\nnext \\\"q\\\"\"\n\
         EMPTY=\n\
         1BAD=x\n\
         no equals sign\n\
         TOKEN=later\n",
    );
    assert_eq!(vars["API_KEY"], "snednao_#m34nr");
    assert_eq!(vars["TOKEN"], "later");
    assert_eq!(vars["SINGLE"], "a \"b\" $c");
    assert_eq!(vars["DOUBLE"], "line\nnext \"q\"");
    assert_eq!(vars["EMPTY"], "");
    assert!(!vars.contains_key("1BAD"));
    assert_eq!(vars.len(), 5);
}

#[test]
fn names_resolve_from_the_projects_env_file() {
    let project = temp("resolve");
    fs::write(project.join(ENV_FILE), "API_KEY=one\nDB=two\n").unwrap();
    let names = vec!["DB".to_string(), "API_KEY".to_string()];
    assert_eq!(
        resolve(&project, &names).unwrap(),
        creds(&[("DB", "two"), ("API_KEY", "one")])
    );
    assert!(resolve(&project, &[]).unwrap().is_empty());
}

#[test]
fn a_missing_name_says_where_to_add_it_without_any_values() {
    let project = temp("missing");
    fs::write(project.join(ENV_FILE), "API_KEY=secret-value\n").unwrap();
    let e = resolve(&project, &["OTHER".into()]).unwrap_err();
    assert_eq!(e.code, ErrorCode::NotFound);
    assert!(e.message.contains("no credential named OTHER"), "{e:?}");
    assert!(e.fix.as_deref().unwrap().contains("OTHER=<value>"));
    assert!(!format!("{e:?}").contains("secret-value"));

    let empty = temp("no-env");
    let e = resolve(&empty, &["API_KEY".into()]).unwrap_err();
    assert_eq!(e.code, ErrorCode::NotFound);

    let e = resolve(&project, &["rm -rf".into()]).unwrap_err();
    assert_eq!(e.code, ErrorCode::BadRequest);
}

#[test]
fn a_printed_value_is_replaced_with_its_name_in_brackets() {
    assert_eq!(
        redact(
            &[("API_KEY_1", "snednao_#m34nr")],
            &["API_KEY_1: snednao_#m34nr\n"]
        ),
        "API_KEY_1: [API_KEY_1]\n"
    );
    assert_eq!(redact(&[("A", "xy")], &["xyxy and xy"]), "[A][A] and [A]");
}

#[test]
fn a_value_split_across_chunks_is_still_replaced() {
    let pairs = [("API_KEY", "snednao_#m34nr")];
    let expected = "key: [API_KEY] done";
    let text = "key: snednao_#m34nr done";
    for cut in 0..=text.len() {
        assert_eq!(
            redact(&pairs, &[&text[..cut], &text[cut..]]),
            expected,
            "{cut}"
        );
    }
    let bytes: Vec<String> = text.chars().map(String::from).collect();
    let chunks: Vec<&str> = bytes.iter().map(String::as_str).collect();
    assert_eq!(redact(&pairs, &chunks), expected);
}

#[test]
fn held_back_text_that_is_not_a_value_comes_out() {
    let mut r = Redactor::new(&creds(&[("K", "secret")]));
    assert_eq!(r.push(b"a sec"), b"a ");
    assert_eq!(r.push(b"ond"), b"second");
    assert_eq!(r.push(b" sec"), b" ");
    assert_eq!(r.finish(), b"sec");
    assert!(!r.replaced());
    assert_eq!(r.push(b"a secret"), b"a [K]");
    assert!(r.replaced());
}

#[test]
fn the_longest_value_wins_and_empty_values_are_ignored() {
    assert_eq!(
        redact(
            &[("SHORT", "abc"), ("LONG", "abcdef"), ("EMPTY", "")],
            &["abcdef abc"]
        ),
        "[LONG] [SHORT]"
    );
    assert_eq!(
        redact(&[("A", "abcd"), ("B", "bc")], &["ab", "cx"]),
        "a[B]x"
    );
}

#[test]
fn without_credentials_output_passes_straight_through() {
    let mut r = Redactor::new(&[]);
    assert_eq!(r.push(b"anything"), b"anything");
    assert!(r.finish().is_empty());
}
