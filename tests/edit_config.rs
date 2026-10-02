#![cfg(unix)]

use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

fn config_path(home: &Path) -> PathBuf {
    let root = if cfg!(target_os = "macos") {
        home.join("Library/Application Support")
    } else {
        home.join("config")
    };

    root.join("dependabot-reviewer/state.toml")
}

fn command(home: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_dependabot-reviewer"));
    command
        .arg("--edit-config")
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env("VISUAL", "unused-visual-editor")
        .env_remove("GITHUB_TOKEN");

    command
}

#[test]
fn creates_config_and_uses_editor_with_quoted_arguments_without_authentication() {
    let home = tempfile::Builder::new()
        .prefix("reviewer config test ")
        .tempdir()
        .expect("temporary home should be created");
    let editor = home.path().join("test editor.sh");
    let capture = home.path().join("editor-arguments");
    fs::write(
        &editor,
        "test -f \"$2\" || exit 1\nprintf '%s\\n' \"$@\" > \"$CAPTURE_PATH\"\nprintf '\\n# edited\\n' >> \"$2\"\n",
    )
    .expect("editor script should be written");

    let output = command(home.path())
        .env(
            "EDITOR",
            format!(
                "sh {} 'argument with spaces'",
                shell_words::quote(editor.to_str().expect("editor path should be UTF-8"))
            ),
        )
        .env("CAPTURE_PATH", &capture)
        .output()
        .expect("command should run");

    assert!(
        output.status.success(),
        "command failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let path = config_path(home.path());
    assert_eq!(
        fs::read_to_string(capture).expect("editor should capture its arguments"),
        format!("argument with spaces\n{}\n", path.display())
    );

    let content = fs::read_to_string(path).expect("config should be created");
    assert!(content.ends_with("# edited\n"));

    let config: toml::Value = toml::from_str(&content).expect("config should contain valid TOML");
    assert_eq!(
        config
            .get("settings")
            .and_then(|settings| settings.get("default_orgs")),
        Some(&toml::Value::Array(Vec::new()))
    );
}

#[test]
fn preserves_existing_config_even_when_toml_is_invalid() {
    let home = tempfile::tempdir().expect("temporary home should be created");
    let path = config_path(home.path());
    fs::create_dir_all(path.parent().expect("config should have a parent"))
        .expect("config directory should be created");
    let content = "[settings\ndefault_orgs = ['example']\n";
    fs::write(&path, content).expect("config should be written");

    let output = command(home.path())
        .env("EDITOR", "true")
        .output()
        .expect("command should run");

    assert!(
        output.status.success(),
        "command failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(path).expect("config should exist"),
        content
    );
}

#[test]
fn reports_editor_failure() {
    let home = tempfile::tempdir().expect("temporary home should be created");
    let output = command(home.path())
        .env("EDITOR", "sh -c 'exit 17' --")
        .output()
        .expect("command should run");

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("17"));
}

#[test]
fn prints_config_path_when_editor_is_not_set() {
    let home = tempfile::tempdir().expect("temporary home should be created");
    let output = command(home.path())
        .env_remove("EDITOR")
        .env("PATH", home.path())
        .output()
        .expect("command should run");

    assert!(
        output.status.success(),
        "command failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        format!(
            "EDITOR is not set. Configuration file: {}\n",
            config_path(home.path()).display()
        )
    );
    assert!(!config_path(home.path()).exists());
}

#[test]
fn reports_missing_editor() {
    let home = tempfile::tempdir().expect("temporary home should be created");
    let editor = home.path().join("missing-editor");
    let output = command(home.path())
        .env("EDITOR", editor.as_os_str())
        .output()
        .expect("command should run");

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("Failed to start editor"));
}

#[test]
fn rejects_empty_or_invalid_editor_commands() {
    let home = tempfile::tempdir().expect("temporary home should be created");

    for editor in ["", "   ", "''", "'unterminated"] {
        let output = command(home.path())
            .env("EDITOR", editor)
            .output()
            .expect("command should run");

        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("EDITOR"));
        assert!(!config_path(home.path()).exists());
    }
}
