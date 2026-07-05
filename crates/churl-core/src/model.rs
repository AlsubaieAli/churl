/// HTTP request method.
///
/// Covers the standard methods used in REST APIs. Additional methods (e.g. CONNECT, TRACE)
/// can be added in later milestones if needed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Method {
    Get,
    Post,
    Put,
    Patch,
    Delete,
    Head,
    Options,
}

impl std::fmt::Display for Method {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Method::Get => "GET",
            Method::Post => "POST",
            Method::Put => "PUT",
            Method::Patch => "PATCH",
            Method::Delete => "DELETE",
            Method::Head => "HEAD",
            Method::Options => "OPTIONS",
        };
        f.write_str(s)
    }
}

/// Error returned when a string cannot be parsed as an HTTP [`Method`].
#[derive(Debug, thiserror::Error)]
#[error("unknown HTTP method: {0:?}")]
pub struct ParseMethodError(String);

impl std::str::FromStr for Method {
    type Err = ParseMethodError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_uppercase().as_str() {
            "GET" => Ok(Method::Get),
            "POST" => Ok(Method::Post),
            "PUT" => Ok(Method::Put),
            "PATCH" => Ok(Method::Patch),
            "DELETE" => Ok(Method::Delete),
            "HEAD" => Ok(Method::Head),
            "OPTIONS" => Ok(Method::Options),
            _ => Err(ParseMethodError(s.to_owned())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn method_round_trip() {
        let methods = [
            Method::Get,
            Method::Post,
            Method::Put,
            Method::Patch,
            Method::Delete,
            Method::Head,
            Method::Options,
        ];
        for method in methods {
            let displayed = method.to_string();
            let parsed = Method::from_str(&displayed)
                .unwrap_or_else(|_| panic!("failed to parse back: {displayed}"));
            assert_eq!(method, parsed, "round-trip failed for {method}");
        }
    }

    #[test]
    fn method_parse_case_insensitive() {
        assert_eq!(Method::from_str("get").unwrap(), Method::Get);
        assert_eq!(Method::from_str("Post").unwrap(), Method::Post);
    }

    #[test]
    fn method_parse_unknown_errors() {
        assert!(Method::from_str("CONNECT").is_err());
    }
}
