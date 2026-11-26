//! Client entry points and wallet behavior.
mod client {
    use kaspa_pong::{menu, node, terminal};

    #[test]
    fn wallet_names_are_explicit_and_ambiguous_names_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        assert!(menu::named_wallet(dir.path(), "left").is_err());
        let legacy = dir.path().join("left.wallet.json");
        // These are deliberately not valid wallets: name discovery must not read keys.
        std::fs::write(&legacy, "private contents must not be read").unwrap();
        assert_eq!(menu::named_wallet(dir.path(), "left").unwrap(), legacy);
        let personal = dir.path().join("wallets/left/wallet.json");
        std::fs::create_dir_all(personal.parent().unwrap()).unwrap();
        std::fs::write(&personal, "private contents must not be read").unwrap();
        assert!(menu::named_wallet(dir.path(), "left").unwrap_err().to_string().contains("ambiguous"));
        assert!(menu::named_wallet(dir.path(), "../left").is_err());
        std::fs::remove_file(&legacy).unwrap();
        assert_eq!(menu::named_wallet(dir.path(), "left").unwrap(), personal);
    }

    #[test]
    fn missing_wallet_is_rejected_before_menu_session_defaults_environment_or_network() {
        use std::process::Command;
        let hex_key = |byte| secp256k1::SecretKey::from_slice(&[byte; 32]).unwrap().display_secret().to_string();
        let dir = tempfile::tempdir().unwrap();
        for name in ["left.wallet.json", "right.wallet.json", "left.session.json"] {
            std::fs::write(dir.path().join(name), "must not be loaded").unwrap();
        }
        for args in [vec![], vec!["host"], vec!["host", "--json"], vec!["join"], vec!["address"]] {
            let output = Command::new(env!("CARGO_BIN_EXE_pong"))
                .args(&args)
                .env("PONG_PLAYER_KEY", hex_key(1))
                .env("PONG_FEE_KEY", hex_key(2))
                .current_dir(dir.path())
                .output()
                .unwrap();
            let error = String::from_utf8_lossy(&output.stderr);
            assert!(!output.status.success(), "must require identity for {args:?}");
            assert!(error.contains("explicit wallet"), "{args:?}: {error}");
            assert!(error.contains("Usage:"));
            assert!(output.stdout.is_empty(), "must not open a game menu or print a default identity");
        }
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 3, "must not create or change session files");
    }

    #[test]
    fn activity_and_reorg_panels_keep_the_court_and_error_status_visible() {
        let mut lines = vec!["title".to_string(), "score".into(), "updater".into(), format!("+{}+", "-".repeat(64))];
        for row in 0..16 {
            lines.push(format!("|court row {row:02}|"));
        }
        lines.extend([
            format!("+{}+", "-".repeat(64)),
            "controls".into(),
            "metrics".into(),
            "Spending paused: fee budget exhausted".into(),
        ]);
        let frame = lines.join("\n");
        for height in 20..=24 {
            let body = terminal::panel_body(&frame, height);
            assert_eq!(body.iter().filter(|line| line.starts_with("|court row")).count(), 16);
            assert_eq!(body.last().copied(), Some("Spending paused: fee budget exhausted"));
            assert!(body.len() <= height);
        }
    }

    #[test]
    fn rendering_updates_only_changed_rows_and_erases_shortened_content_without_scrolling() {
        use terminal::ScreenFrame;
        let old = ScreenFrame::compose("PONG\n| ball    |\nlong status", None, None, (80, 25));
        assert_eq!(old.changed_rows(None).len(), 25);
        let same = ScreenFrame::compose("PONG\n| ball    |\nlong status", None, None, (80, 25));
        assert!(same.changed_rows(Some(&old)).is_empty());
        let next = ScreenFrame::compose("PONG\n|    ball |\nOK", None, None, (80, 25));
        assert_eq!(next.changed_rows(Some(&old)), vec![1, 2]);
        assert!(next.rows[2].text.starts_with("OK       "));
        assert!(next.rows.iter().all(|r| r.text.chars().count() == 79 && !r.text.contains(['\n', '\r'])));
        let resized = ScreenFrame::compose("PONG", None, None, (70, 20));
        assert_eq!(resized.changed_rows(Some(&next)).len(), 20);
    }

    #[test]
    fn rendering_removes_activity_and_reorg_colours_when_panels_disappear() {
        use terminal::{RowStyle, ScreenFrame};
        let with = ScreenFrame::compose("PONG", Some("ACCEPTED\ntxid"), Some("REORG OCCURRED\nrollback\nPress R"), (80, 25));
        assert_eq!(with.rows[20].style, RowStyle::Activity);
        assert_eq!(with.rows[22].style, RowStyle::Alert);
        let without = ScreenFrame::compose("PONG", None, None, (80, 25));
        assert_eq!(without.changed_rows(Some(&with)), vec![20, 21, 22, 23, 24]);
        assert!(without.rows[20..].iter().all(|r| r.style == RowStyle::Plain && r.text.trim().is_empty()));
    }

    #[test]
    fn wallet_generation_and_reload_preserve_the_player_key_without_replacing_funded_files() {
        use node::wallet::{Wallet, public_identity};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("player.wallet.json");
        let generated = Wallet::generate().unwrap();
        let independent = Wallet::generate().unwrap();
        assert!(generated.player != independent.player);
        generated.save_new(&path).unwrap();
        let original = std::fs::read(&path).unwrap();
        let loaded = Wallet::load(&path).unwrap();
        assert_eq!(loaded.player, generated.player);
        let public = public_identity("testnet-10", &loaded.player).unwrap();
        assert!(public["player_address"].as_str().unwrap().starts_with("kaspatest:"));
        assert!(!public.to_string().contains(&generated.player.secret_key().display_secret().to_string()));
        assert!(independent.save_new(&path).is_err());
        assert!(std::fs::read(&path).unwrap() == original);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o077, 0);
        }
        std::fs::write(&path, r#"{"version":1,"player_key":"secret-that-must-not-be-echoed"}"#).unwrap();
        let error = Wallet::load(&path).err().unwrap().to_string();
        assert!(!error.contains("secret-that-must-not-be-echoed"));
    }

    #[test]
    fn wallet_cli_generates_and_reuses_an_address_offline_without_exposing_secrets() {
        use std::process::Command;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("player.wallet.json");
        let mut command = Command::new(env!("CARGO_BIN_EXE_pong"));
        command
            .current_dir(dir.path())
            .args(["--network", "testnet-10", "--endpoint", "grpc://127.0.0.1:1", "keygen", "--out"])
            .arg(&path);
        let generated = command.output().unwrap();
        assert!(generated.status.success(), "{}", String::from_utf8_lossy(&generated.stderr));
        let public: serde_json::Value = serde_json::from_slice(&generated.stdout).unwrap();
        assert_eq!(public["network"], "testnet-10");
        assert_eq!(public["player_address"], public["address"]);
        assert!(public["address"].as_str().unwrap().starts_with("kaspatest:"));
        let wallet = node::wallet::Wallet::load(&path).unwrap();
        let secret = wallet.player.secret_key().display_secret().to_string();
        assert!(!String::from_utf8_lossy(&generated.stdout).contains(&secret));
        assert!(!String::from_utf8_lossy(&generated.stderr).contains(&secret));
        let reused = Command::new(env!("CARGO_BIN_EXE_pong"))
            .current_dir(dir.path())
            .env("PONG_PLAYER_KEY", "invalid-env-must-be-ignored")
            .env("PONG_FEE_KEY", "invalid-env-must-be-ignored")
            .args(["--network", "testnet-10", "--wallet"])
            .arg(&path)
            .arg("address")
            .output()
            .unwrap();
        assert!(reused.status.success(), "{}", String::from_utf8_lossy(&reused.stderr));
        let reused: serde_json::Value = serde_json::from_slice(&reused.stdout).unwrap();
        assert_eq!(reused["player"], public["player"]);
        assert_eq!(reused["address"], public["address"]);
        assert_eq!(reused["player_address"], public["player_address"]);
        let original = std::fs::read(&path).unwrap();
        assert!(!command.output().unwrap().status.success());
        assert!(std::fs::read(&path).unwrap() == original);
    }

    #[test]
    fn config_accepts_grpc_borsh_and_explicit_public_fallbacks() {
        let mut c = node::Config::default();
        for endpoint in
            ["grpc://127.0.0.1:16610", "ws://127.0.0.1:17610", "wss://node.example/kaspa/testnet-10/wrpc/borsh", "resolver"]
        {
            c.endpoint = endpoint.into();
            c.validate().unwrap();
        }
        c.fallback_endpoints = vec!["wss://node.example/kaspa/testnet-10/wrpc/borsh".into()];
        c.resolver_urls = vec!["https://resolver.example".into()];
        c.validate().unwrap();
        c.fallback_endpoints.push("https://not-a-websocket.example".into());
        assert!(c.validate().is_err());
        c.fallback_endpoints.clear();
        c.endpoint = "https://not-a-websocket.example".into();
        assert!(c.validate().is_err());
    }
}
