use super::*;

const MINIMAL: &str = r#"[vm]
image   = "base"        # pinned to a digest on first run
cpus    = 2
memory  = "2G"

[storage]
mode    = "sealed"      # or "synced"

[network]
mode    = "nat"         # off | nat
"#;

const DIGEST: &str = "3f2a9c1e0000000000000000000000000000000000000000000000000000abcd";

fn err(text: &str) -> ConfigError {
    parse(text).unwrap_err()
}

#[test]
fn the_file_cave_init_writes_gives_the_defaults() {
    assert_eq!(parse(MINIMAL).unwrap(), Config::default());
    assert_eq!(parse("").unwrap(), Config::default());
}

#[test]
fn every_key_is_read() {
    let text = r#"
[vm]
image        = "ubuntu@sha256:3f2a9c1e0000000000000000000000000000000000000000000000000000abcd"
cpus         = 4
memory       = "512M"
idle_suspend = "off"
ttl          = "8h"

[storage]
mode         = "synced"
disk         = "1T"

[network]
mode         = "off"

[snapshots]
auto         = false
keep         = 0

[updates]
check        = false
"#;
    let config = parse(text).unwrap();
    assert_eq!(
        config,
        Config {
            image: format!("ubuntu@sha256:{DIGEST}"),
            cpus: 4,
            memory_bytes: 512 << 20,
            idle_suspend: None,
            ttl: Some(Duration::from_secs(8 * 3600)),
            storage: Storage::Synced,
            disk_bytes: 1 << 40,
            net: NetMode::Off,
            snapshots_auto: false,
            snapshots_keep: 0,
            updates_check: false,
        }
    );
    assert_eq!(config.image_name(), "ubuntu");
    assert_eq!(config.image_digest(), Some(DIGEST));
    assert_eq!(Config::default().image_digest(), None);
}

#[test]
fn dotted_and_inline_tables_work_too() {
    let config = parse("vm.cpus = 3\nnetwork = { mode = \"off\" }\n").unwrap();
    assert_eq!(config.cpus, 3);
    assert_eq!(config.net, NetMode::Off);
}

#[test]
fn a_size_without_a_unit_is_the_documented_error() {
    let e = err("[vm]\nimage = \"base\"\ncpus = 2\nmemory = 2048\n");
    assert_eq!(
        e.to_string(),
        "cave.toml line 4: memory = 2048 needs a unit"
    );
    assert_eq!(e.fix, "write memory = \"2G\" (units: K, M, G, T)");
}

#[test]
fn lan_in_the_file_is_the_documented_error() {
    let text = format!("{MINIMAL}\n\n[snapshots]\n").replace("mode    = \"nat\"", "mode = \"lan\"");
    let e = err(&text);
    assert_eq!(
        e.to_string(),
        "cave.toml line 10: network mode \"lan\" cannot be set in cave.toml"
    );
    assert_eq!(e.fix, "pass --net lan to cave run when you need LAN access");
}

#[test]
fn unknown_keys_and_sections_are_errors() {
    let e = err("[vm]\ncpu = 2\n");
    assert_eq!(e.to_string(), "cave.toml line 2: unknown key cpu in [vm]");
    assert_eq!(
        e.fix,
        "the keys in [vm] are image, cpus, memory, idle_suspend and ttl"
    );

    let e = err("[vm]\n\n[ports]\nweb = 3000\n");
    assert_eq!(e.to_string(), "cave.toml line 3: unknown section [ports]");

    let e = err("vm = 3\n");
    assert_eq!(e.to_string(), "cave.toml line 1: vm must be a table");
}

#[test]
fn wrong_values_say_how_to_fix_them() {
    let cases = [
        (
            "[vm]\ncpus = 0\n",
            "cpus must be a whole number of at least 1",
            "write cpus = 2",
        ),
        (
            "[vm]\ncpus = \"2\"\n",
            "cpus must be a whole number of at least 1",
            "write cpus = 2",
        ),
        (
            "[vm]\nmemory = \"2GB\"\n",
            "memory = \"2GB\" is not a size",
            "write memory = \"2G\" (units: K, M, G, T)",
        ),
        (
            "[vm]\nmemory = \"0G\"\n",
            "memory = \"0G\" is not a size",
            "write memory = \"2G\" (units: K, M, G, T)",
        ),
        (
            "[vm]\nttl = 60\n",
            "ttl = 60 needs a unit",
            "write ttl = \"8h\" (units: s, m, h, d) or ttl = \"off\"",
        ),
        (
            "[vm]\nidle_suspend = \"soon\"\n",
            "idle_suspend = \"soon\" is not a duration",
            "write idle_suspend = \"30m\" (units: s, m, h, d) or idle_suspend = \"off\"",
        ),
        (
            "[vm]\nimage = \"Base\"\n",
            "image = \"Base\" is not an image name",
            "write image = \"base\", or a name from `cave images`",
        ),
        (
            "[vm]\nimage = \"base@sha256:abc\"\n",
            "image = \"base@sha256:abc\" is not an image name",
            "write image = \"base\", or a name from `cave images`",
        ),
        (
            "[storage]\nmode = \"mirror\"\n",
            "storage mode \"mirror\" is not a mode",
            "write mode = \"sealed\" or mode = \"synced\"",
        ),
        (
            "[network]\nmode = \"bridge\"\n",
            "network mode \"bridge\" is not a mode",
            "write mode = \"nat\" or mode = \"off\"",
        ),
        (
            "[snapshots]\nauto = \"yes\"\n",
            "auto must be true or false",
            "write auto = true or auto = false",
        ),
    ];
    for (text, message, fix) in cases {
        let e = err(text);
        assert_eq!(e.message, message, "{text}");
        assert_eq!(e.fix, fix, "{text}");
        assert_eq!(e.line, Some(2), "{text}");
    }
}

#[test]
fn bad_syntax_names_the_line() {
    let e = err("[vm]\ncpus = = 2\n");
    assert_eq!(e.line, Some(2));
    assert!(e.fix.contains("TOML syntax"));
}

#[test]
fn sizes_and_durations() {
    assert_eq!(parse_size("512K"), Some(512 << 10));
    assert_eq!(parse_size("2g"), Some(2 << 30));
    assert_eq!(parse_size("2"), None);
    assert_eq!(parse_size("1.5G"), None);
    assert_eq!(parse_size("G"), None);
    assert_eq!(parse_size("99999999999T"), None, "overflow");
    assert_eq!(parse_duration("90s"), Some(Duration::from_secs(90)));
    assert_eq!(parse_duration("2d"), Some(Duration::from_secs(172_800)));
    assert_eq!(parse_duration("30M"), None);
    assert_eq!(parse_duration("5"), None);
}

#[test]
fn pinning_changes_only_the_image_value() {
    let pinned = format!("base@sha256:{DIGEST}");
    let out = pin_image(MINIMAL, &pinned).unwrap();
    assert_eq!(
        out,
        MINIMAL.replace("\"base\"        #", &format!("\"{pinned}\"        #"))
    );
    assert_eq!(parse(&out).unwrap().image, pinned);
    // Pinning again replaces the old pin.
    let again = pin_image(
        &out,
        "base@sha256:0000000000000000000000000000000000000000000000000000000000000000",
    )
    .unwrap();
    assert!(again.contains("# pinned to a digest on first run"));
    assert_eq!(again.matches("sha256:").count(), 1);
}

#[test]
fn pinning_adds_the_key_when_it_is_missing() {
    let out = pin_image(
        "# my project\n[network]\nmode = \"off\"\n",
        "base@sha256:00",
    )
    .unwrap();
    assert!(
        out.starts_with("# my project\n[network]\nmode = \"off\"\n"),
        "{out}"
    );
    assert!(out.contains("[vm]\nimage = \"base@sha256:00\"\n"), "{out}");
}

#[test]
fn load_reads_the_project_file() {
    let dir = std::env::temp_dir().join(format!("cave-config-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let e = load(&dir).unwrap_err();
    assert!(e.message.starts_with("cannot read"), "{e}");
    assert_eq!(e.fix, "Run `cave init` to create it");
    std::fs::write(dir.join(FILE), "[vm]\ncpus = 6\n").unwrap();
    assert_eq!(load(&dir).unwrap().cpus, 6);
    std::fs::remove_dir_all(&dir).unwrap();
}
