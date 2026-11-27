use std::path::Path;
use std::process::{Command, Output};

fn complete(shell: &str, config: &Path, words: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_mihoctl"))
        .env("MIHOCTL_COMPLETE", shell)
        .env("MIHOCTL_CONFIG", config)
        .env("_CLAP_COMPLETE_INDEX", words.len().to_string())
        .env_remove("_CLAP_IFS")
        .args(["--", "mihoctl"])
        .args(words)
        .output()
        .unwrap()
}

#[test]
fn subscription_completion_uses_the_active_local_config() {
    let dir = std::env::temp_dir().join(format!("mihoctl-completion-{}", std::process::id()));
    std::fs::create_dir(&dir).unwrap();
    let default = dir.join("default.toml");
    let alternate = dir.join("other config.toml");
    let header = "template = 'template.yaml'\nsecret = 'completion-secret'\ncore_dir = 'core'\n";
    let subscription = |name| format!("[[subscription]]\nname = '{name}'\nurl = 'https://sub.example/token'\n");
    std::fs::write(&default, format!("{header}{}{}", subscription("alpha-east"), subscription("alpha-west"))).unwrap();
    std::fs::write(&alternate, format!("{header}{}", subscription("beta"))).unwrap();
    let other = alternate.to_str().unwrap();
    let quoted = shlex::try_quote(other).unwrap();

    let cases = [
        (complete("fish", &default, &["update", "alpha-east", "alpha-w"]), "alpha-west\n"),
        (complete("fish", &default, &["history", "--config", other, "be"]), "beta\n"),
        (complete("bash", &default, &["-c", other, "restore", "be"]), "beta"),
        (complete("bash", &default, &["--config", &quoted, "restore", "be"]), "beta"),
        (complete("fish", &dir.join("missing.toml"), &["update", "al"]), ""),
    ];
    let created_core = dir.join("core").exists();
    std::fs::remove_dir_all(&dir).unwrap();

    for (output, expected) in cases {
        assert!(output.status.success(), "{output:?}");
        assert!(output.stderr.is_empty(), "{output:?}");
        assert_eq!(String::from_utf8(output.stdout).unwrap(), expected);
    }
    assert!(!created_core);
}
