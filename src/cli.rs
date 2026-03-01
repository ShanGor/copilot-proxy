#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StartupArgs {
    pub proxy: Option<String>,
}

pub fn parse_startup_args<I>(args: I) -> Result<StartupArgs, String>
where
    I: IntoIterator<Item = String>,
{
    let mut out = StartupArgs::default();
    let mut it = args.into_iter().peekable();

    while let Some(arg) = it.next() {
        if arg == "--help" || arg == "-h" {
            return Err(usage());
        }

        if arg == "--proxy" {
            let Some(val) = it.next() else {
                return Err("missing value for --proxy\n\n".to_string() + &usage());
            };
            out.proxy = Some(val);
            continue;
        }

        if let Some(val) = arg.strip_prefix("--proxy=") {
            if val.is_empty() {
                return Err("missing value for --proxy\n\n".to_string() + &usage());
            }
            out.proxy = Some(val.to_string());
            continue;
        }

        return Err(format!("unknown argument: {arg}\n\n{}", usage()));
    }

    Ok(out)
}

pub fn usage() -> String {
    "Usage: copilot_proxy [--proxy <PROXY_URL>]\n\nExample:\n  copilot_proxy --proxy http://proxy.company.local:8080\n"
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::parse_startup_args;

    #[test]
    fn parses_proxy_with_space() {
        let parsed =
            parse_startup_args(vec!["--proxy".to_string(), "http://proxy:8080".to_string()])
                .expect("parse args");
        assert_eq!(parsed.proxy.as_deref(), Some("http://proxy:8080"));
    }

    #[test]
    fn parses_proxy_with_equals() {
        let parsed = parse_startup_args(vec!["--proxy=http://proxy:8080".to_string()])
            .expect("parse args");
        assert_eq!(parsed.proxy.as_deref(), Some("http://proxy:8080"));
    }

    #[test]
    fn errors_on_missing_proxy_value() {
        let err = parse_startup_args(vec!["--proxy".to_string()]).expect_err("must fail");
        assert!(err.contains("missing value for --proxy"));
    }

    #[test]
    fn errors_on_unknown_arg() {
        let err = parse_startup_args(vec!["--bad".to_string()]).expect_err("must fail");
        assert!(err.contains("unknown argument"));
    }
}
