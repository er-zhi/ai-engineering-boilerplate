// Proves Gateway configuration rejects invalid values before startup I/O.

    use super::*;

    #[test]
    fn endpoints_require_an_absolute_http_uri_within_the_limit() {
        for invalid in [
            String::new(),
            "localhost:8081".to_owned(),
            "ftp://example.com".to_owned(),
            "http:///missing-host".to_owned(),
            "x".repeat(MAX_ENDPOINT_URL_CHARS + 1),
        ] {
            assert!(endpoint("CRAWLER_URL", invalid).is_err());
        }
        assert!(endpoint("CRAWLER_URL", "http://crawler:8081".to_owned()).is_ok());
    }

    #[test]
    fn database_url_requires_postgres_with_a_host() {
        for invalid in ["", "not a URL", "mysql://db/app", "postgres:///app"] {
            assert!(validate_database_url(invalid).is_err(), "{invalid}");
        }
        assert!(validate_database_url("postgres://gateway@db/app").is_ok());
        assert!(validate_database_url("postgresql://gateway@db/app").is_ok());
    }

    #[test]
    fn bounded_values_reject_empty_and_one_past_the_limit() {
        assert!(bounded("VALUE", String::new(), 4).is_err());
        assert_eq!(bounded("VALUE", "four".to_owned(), 4).unwrap(), "four");
        assert!(bounded("VALUE", "fives".to_owned(), 4).is_err());
    }

    #[test]
    fn session_ttl_accepts_its_boundaries_and_rejects_values_outside_them() {
        assert_eq!(
            session_ttl_from(None).unwrap(),
            Duration::from_secs(DEFAULT_SESSION_TTL_HOURS * 3_600)
        );
        assert_eq!(
            session_ttl_from(Some("1".to_owned())).unwrap(),
            Duration::from_secs(3_600)
        );
        assert!(session_ttl_from(Some("0".to_owned())).is_err());
        assert!(session_ttl_from(Some((MAX_SESSION_TTL_HOURS + 1).to_string())).is_err());
        assert!(session_ttl_from(Some("not-a-number".to_owned())).is_err());
    }

    #[test]
    fn optional_environment_values_default_only_when_absent() {
        assert_eq!(
            optional_value("VALUE", Err(std::env::VarError::NotPresent)).unwrap(),
            None
        );
        let invalid = std::ffi::OsString::from("invalid");
        let error = optional_value("VALUE", Err(std::env::VarError::NotUnicode(invalid)))
            .unwrap_err();
        assert!(error.contains("VALUE"));
        assert!(error.contains("non-Unicode"));
    }

    #[test]
    fn validated_config_exposes_only_read_only_values() {
        let config = GatewayConfig {
            crawler_url: "http://crawler:8081".parse().unwrap(),
            knowledge_base_url: "http://knowledge-base:8084".parse().unwrap(),
            chat_url: "http://chat:8088".parse().unwrap(),
            frontend_dir: PathBuf::from("/frontend"),
            database_url: "postgres://gateway@postgres/gateway".to_owned(),
            password: "secret".to_owned(),
            session_ttl: Duration::from_secs(3_600),
        };

        assert_eq!(config.crawler_url().to_string(), "http://crawler:8081/");
        assert_eq!(
            config.knowledge_base_url().to_string(),
            "http://knowledge-base:8084/"
        );
        assert_eq!(config.frontend_dir(), &PathBuf::from("/frontend"));
        assert_eq!(
            config.database_url(),
            "postgres://gateway@postgres/gateway"
        );
        assert_eq!(config.password(), "secret");
        assert_eq!(config.session_ttl(), Duration::from_secs(3_600));
    }
