use super::{Args, ImportError, Parser};

impl Parser {
    /// Handles one `--flag` (with an optional inline `=value`).
    pub(super) fn long_flag(
        &mut self,
        name: &str,
        inline_value: Option<String>,
        args: &mut Args,
    ) -> Result<(), ImportError> {
        let value = |parser_args: &mut Args| -> Result<String, ImportError> {
            match inline_value.clone() {
                Some(value) => Ok(value),
                None => parser_args
                    .next()
                    .ok_or_else(|| ImportError::MissingValue(format!("--{name}"))),
            }
        };
        match name {
            "request" => self.set_method(&value(args)?),
            "header" => {
                self.add_header(&value(args)?);
                Ok(())
            }
            "data" | "data-raw" | "data-ascii" | "data-binary" => self.add_data(value(args)?),
            "json" => self.add_json(value(args)?),
            "form" => Err(ImportError::Unsupported(
                "multipart (-F) — model has no multipart body yet".to_owned(),
            )),
            "user" => {
                self.add_basic_auth(&value(args)?);
                Ok(())
            }
            "url" => self.set_url(value(args)?),
            "output" => {
                let file = value(args)?;
                self.warnings
                    .push(format!("ignored: -o output file {file:?} discarded"));
                Ok(())
            }
            // Output/verbosity flags with no request semantics: accepted silently.
            // `--location` matches behaviour — the reqwest client already follows
            // redirects by default.
            "location" | "silent" | "verbose" | "show-error" => Ok(()),
            "compressed" => {
                self.warnings
                    .push("ignored: compression negotiation not configured".to_owned());
                Ok(())
            }
            "insecure" => {
                self.warnings
                    .push("ignored: TLS verification is always on".to_owned());
                Ok(())
            }
            _ => Err(ImportError::UnknownFlag(format!("--{name}"))),
        }
    }

    /// Handles one short token: a single flag (`-X`), a cluster of value-less
    /// flags (`-sSL`), or a flag with its value attached (`-XPOST`). A
    /// value-taking flag consumes the rest of the token (or the next argument)
    /// and ends the cluster, matching curl.
    pub(super) fn short_cluster(
        &mut self,
        token: &str,
        args: &mut Args,
    ) -> Result<(), ImportError> {
        let chars: Vec<char> = token[1..].chars().collect();
        for (index, &c) in chars.iter().enumerate() {
            match c {
                // Value-less flags: -L follows-redirects (already client default),
                // -s/-v/-S are output/verbosity noise.
                'L' | 's' | 'v' | 'S' => {}
                'k' => self
                    .warnings
                    .push("ignored: TLS verification is always on".to_owned()),
                'X' | 'H' | 'd' | 'u' | 'o' | 'F' => {
                    let rest: String = chars[index + 1..].iter().collect();
                    let value = if rest.is_empty() {
                        args.next()
                            .ok_or_else(|| ImportError::MissingValue(format!("-{c}")))?
                    } else {
                        rest
                    };
                    return match c {
                        'X' => self.set_method(&value),
                        'H' => {
                            self.add_header(&value);
                            Ok(())
                        }
                        'd' => self.add_data(value),
                        'u' => {
                            self.add_basic_auth(&value);
                            Ok(())
                        }
                        'o' => {
                            self.warnings
                                .push(format!("ignored: -o output file {value:?} discarded"));
                            Ok(())
                        }
                        'F' => Err(ImportError::Unsupported(
                            "multipart (-F) — model has no multipart body yet".to_owned(),
                        )),
                        _ => unreachable!("outer match already narrowed the flag"),
                    };
                }
                _ => return Err(ImportError::UnknownFlag(format!("-{c}"))),
            }
        }
        Ok(())
    }

    fn set_method(&mut self, value: &str) -> Result<(), ImportError> {
        self.method = Some(
            value
                .parse()
                .map_err(|_| ImportError::InvalidMethod(value.to_owned()))?,
        );
        Ok(())
    }

    fn add_data(&mut self, value: String) -> Result<(), ImportError> {
        if value.starts_with('@') {
            return Err(ImportError::Unsupported("@file body".to_owned()));
        }
        self.data_parts.push(value);
        Ok(())
    }

    fn add_json(&mut self, value: String) -> Result<(), ImportError> {
        self.add_data(value)?;
        self.json = true;
        Ok(())
    }
}
